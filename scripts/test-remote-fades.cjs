'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const Fades = require('../crates/fsonos-api/src/remote/fades.js');
const group = (changes = {}) => ({ household: 'home', coordinator_room: 'Kitchen', members: ['Kitchen', 'Study'], ...changes });
const zones = () => [group(), group({ household: 'other', members: ['Kitchen'] })];
const sound = (changes = {}) => ({ volume: 18, mute: false, transport_state: 'playing',
  track: { uri: 'x-test:music', queue_position: 2, title: 'Example music', position_secs: 20 }, ...changes });
const clone = (value) => structuredClone(value);
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const tick = () => new Promise((resolve) => setImmediate(resolve));

function fixture() {
  const calls = [];
  let inventory = zones(), view = sound(), handler = null, publications = 0;
  const answer = (target) => ({ zone: inventory.find((g) => g.members.some((room) => `${room}@${g.household}` === target)), ...clone(view) });
  const m = Fades.model({ changed: () => { publications++; }, call: async (route, args) => {
    calls.push({ route, args: clone(args) });
    if (handler) return handler(route, args);
    if (route === 'zones') return clone(inventory);
    if (route === 'fadeState') return answer(args.params.room);
    if (route === 'fadeVolume') return { done: 'Volume fade completed.', changed: true, volume: args.body.volume };
    if (route === 'fadePause') return { done: 'Group paused and volumes restored.', changed: true };
    throw new Error(`Unexpected route: ${route}`);
  } });
  return { m, calls, answer,
    inventory(value) { inventory = value; }, sound(value) { view = value; }, handle(value) { handler = value; },
    publications: () => publications,
    writes: () => calls.filter((c) => c.route === 'fadeVolume' || c.route === 'fadePause'),
    async ready(mode = 'volume', target = mode === 'volume' ? 'Study@home' : 'Kitchen@home') {
      await m.refresh(); m.mode(mode); await m.read(target);
      if (mode === 'volume') m.edit('level', '25');
      return m;
    },
  };
}

test('mount is lazy; volume targets every room and pause targets whole groups without implicit selection', async () => {
  const f = fixture(), m = f.m;
  assert.equal(f.calls.length, 0);
  assert.equal(await m.refresh(), true);
  assert.deepEqual(m.targets().map((t) => t.target), ['Kitchen@home', 'Study@home', 'Kitchen@other']);
  assert.equal(m.state.target, '');
  assert.equal(m.mode('pause'), true);
  assert.deepEqual(m.targets().map((t) => t.target), ['Kitchen@home', 'Kitchen@other']);
  assert.equal(m.targets()[0].label, 'Kitchen + Study (home)');
  assert.equal(await m.read('Study@home'), false, 'a follower is not presented as a whole group');
  assert.equal(f.calls.length, 1);
});

test('a reviewed volume fade sends one native command for the chosen member with typed values', async () => {
  const f = fixture(), m = await f.ready();
  m.edit('seconds', '12');
  assert.equal(f.calls.length, 2);
  assert.equal(m.review(), true);
  const reviewed = m.state.pending;
  assert.deepEqual(reviewed.body, { zone: 'Study@home', fade_secs: 12, volume: 25 });
  assert.equal(f.writes().length, 0);
  assert.equal(await m.confirm(reviewed), true);
  assert.deepEqual(f.calls.slice(2).map((c) => c.route), ['zones', 'fadeState', 'fadeVolume']);
  assert.deepEqual(f.writes()[0].args.body, reviewed.body);
  assert.equal(m.state.current, null, 'another fade requires a fresh read');
  assert.equal(await m.confirm(reviewed), false);
  assert.equal(f.writes().length, 1);
});

test('gentle pause preserves the group scope and never supplies volume, resume or a browser-side ramp', async () => {
  const f = fixture(), m = await f.ready('pause');
  m.edit('seconds', '60'); m.review();
  assert.equal(await m.confirm(), true);
  assert.deepEqual(f.writes(), [{ route: 'fadePause', args: { body: { zone: 'Kitchen@home', fade_secs: 60 } } }]);
  assert.match(m.state.message, /Group paused and volumes restored/);
});

test('the same named room in another household stays a different target', async () => {
  const f = fixture(), m = await f.ready('volume', 'Kitchen@other');
  m.review(); assert.equal(await m.confirm(), true);
  assert.equal(f.writes()[0].args.body.zone, 'Kitchen@other');
});

test('zero is a valid target volume; duration must be explicit and within 1–60 seconds', async () => {
  const f = fixture(), m = await f.ready();
  m.edit('level', '0'); m.edit('seconds', '1');
  assert.equal(m.review(), true);
  assert.equal(await m.confirm(), true);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Study@home', fade_secs: 1, volume: 0 });
});

test('incomplete or nondecimal numeric drafts never become commands', async () => {
  const f = fixture(), m = await f.ready();
  for (const invalid of ['', '0', '61', '-1', '+2', '1.5', '1e1', ' 5', '5 ', '0x10', 'NaN', '1000']) {
    m.edit('seconds', invalid);
    assert.equal(m.canReview(), false, invalid);
    assert.equal(m.review(), false);
  }
  m.edit('seconds', '5');
  for (const invalid of ['', '-1', '101', '+2', '2.5', '1e1', ' 3', '3 ', 'NaN', '1000']) {
    m.edit('level', invalid); assert.equal(m.canReview(), false, invalid);
  }
  assert.equal(m.edit('level', 20), false);
  assert.equal(m.edit('constructor', '3'), false);
  assert.equal(m.mode('unknown'), false);
  assert.equal(f.writes().length, 0);
});

test('an unchanged target volume is a no-op, and reading resets an old volume draft', async () => {
  const f = fixture(), m = await f.ready();
  m.edit('level', '18'); assert.equal(m.canReview(), false);
  m.edit('level', '25'); assert.equal(m.canReview(), true);
  await m.read(); assert.equal(m.state.level, '18'); assert.equal(m.canReview(), false);
});

test('paused, stopped, transitioning, unknown or unidentified playback cannot be resumed by gentle pause', async () => {
  for (const value of [sound({ transport_state: 'paused' }), sound({ transport_state: 'stopped' }),
    sound({ transport_state: 'transitioning' }), sound({ transport_state: 'unknown' }),
    sound({ track: null }), sound({ track: { uri: '' } }), sound({ track: { uri: 'NOT_IMPLEMENTED' } })]) {
    const f = fixture(); f.sound(value); const m = await f.ready('pause');
    assert.equal(m.canReview(), false);
    assert.equal(m.review(), false);
    assert.equal(await m.confirm(), false);
    assert.equal(f.writes().length, 0);
  }
});

test('volume fading remains available for muted or paused speakers without changing either state', async () => {
  const f = fixture(); f.sound(sound({ mute: true, transport_state: 'paused', track: null }));
  const m = await f.ready(); m.review();
  assert.equal(await m.confirm(), true);
  assert.deepEqual(Object.keys(f.writes()[0].args.body).sort(), ['fade_secs', 'volume', 'zone']);
});

test('cancel, edited inputs and action changes invalidate captured confirmation handlers', async () => {
  const f = fixture(), m = await f.ready();
  m.review(); const first = m.state.pending; m.cancel();
  assert.equal(await m.confirm(first), false);
  m.review(); const second = m.state.pending; m.edit('seconds', '10');
  assert.equal(await m.confirm(second), false);
  m.review(); const third = m.state.pending;
  assert.equal(await m.confirm(second), false, 'an old button cannot confirm the newer review');
  m.mode('pause');
  assert.equal(await m.confirm(third), false);
  assert.equal(m.state.target, '');
  assert.equal(m.state.current, null);
  assert.equal(f.writes().length, 0);
});

test('reviewed inputs and group membership are immutable', async () => {
  const f = fixture(), m = await f.ready('pause'); m.review();
  const p = m.state.pending;
  for (const value of [p, p.body, p.before, p.entry, p.entry.group, p.entry.group.members]) assert.equal(Object.isFrozen(value), true);
  assert.throws(() => { p.body.zone = 'other'; }, TypeError);
  assert.throws(() => { p.entry.group.members.push('another'); }, TypeError);
});

test('changed volume or mute rejects either type of reviewed fade before dispatch', async () => {
  for (const mode of ['volume', 'pause']) for (const change of [{ volume: 19 }, { mute: true }]) {
    const f = fixture(), m = await f.ready(mode); m.review();
    f.sound(sound(change));
    assert.equal(await m.confirm(), false);
    assert.equal(f.writes().length, 0);
    assert.equal(m.state.current, null);
    assert.match(m.state.message, /No fade was sent/);
  }
});

test('a regroup, coordinator switch or disappeared group invalidates gentle pause', async () => {
  for (const inventory of [[group({ members: ['Kitchen'] })],
    [group({ coordinator_room: 'Study' })], [zones()[1]]]) {
    const f = fixture(), m = await f.ready('pause'); m.review(); f.inventory(inventory);
    assert.equal(await m.confirm(), false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.calls.filter((c) => c.route === 'fadeState').length, 1, 'stop before the state read');
  }
});

test('membership ordering alone is not a group change', async () => {
  const f = fixture(), m = await f.ready('pause'); m.review();
  f.inventory([group({ members: ['Study', 'Kitchen'] }), zones()[1]]);
  assert.equal(await m.confirm(), true);
});

test('a per-speaker fade can follow that room through regrouping but not disappearance', async () => {
  const f = fixture(), m = await f.ready(); m.review();
  f.inventory([group({ coordinator_room: 'Study' })]);
  assert.equal(await m.confirm(), true);
  const gone = fixture(), g = await gone.ready(); g.review(); gone.inventory([zones()[1]]);
  assert.equal(await g.confirm(), false);
  assert.equal(gone.writes().length, 0);
});

test('gentle pause rejects a changed source, duplicate-track slot or external pause', async () => {
  for (const change of [sound({ track: { uri: 'x-test:other', queue_position: 2 } }),
    sound({ track: { uri: 'x-test:music', queue_position: 3 } }), sound({ transport_state: 'paused' })]) {
    const f = fixture(), m = await f.ready('pause'); m.review(); f.sound(change);
    assert.equal(await m.confirm(), false);
    assert.equal(f.writes().length, 0);
  }
});

test('normal progress in the same track does not invalidate pause, nor does a track change invalidate a volume-only intent', async () => {
  const f = fixture(), m = await f.ready('pause'); m.review();
  f.sound(sound({ track: { uri: 'x-test:music', queue_position: 2, position_secs: 25 } }));
  assert.equal(await m.confirm(), true);
  const next = fixture(), n = await next.ready(); n.review();
  next.sound(sound({ track: { uri: 'x-test:other', queue_position: 3 } }));
  assert.equal(await n.confirm(), true);
});

test('invalid, duplicate and ambiguous room descriptions fail closed', async () => {
  for (const value of [null, {}, [null], [group({ members: [] })], [group({ household: '' })],
    [group({ coordinator_room: 'Missing' })], [group({ members: ['Kitchen', 'Kitchen'] })],
    [group({ members: ['Kitchen', 1] })], [group(), group()],
    [group({ household: 'b@c', coordinator_room: 'a', members: ['a'] }),
      group({ household: 'c', coordinator_room: 'a@b', members: ['a@b'] })]]) {
    const f = fixture(); f.inventory(value);
    assert.equal(await f.m.refresh(), false, JSON.stringify(value));
    assert.deepEqual(f.m.state.groups, []);
    assert.equal(f.m.state.loading, false);
    assert.equal(f.writes().length, 0);
  }
});

test('missing volume, mute, group or malformed source state cannot enable a fade', async () => {
  const invalid = [null, {}, { zone: group() }, { zone: group({ household: 'wrong' }), ...sound() },
    { zone: group(), ...sound({ volume: -1 }) }, { zone: group(), ...sound({ volume: '18' }) },
    { zone: group(), ...sound({ mute: 0 }) }, { zone: group(), ...sound({ transport_state: 'PLAYING' }) },
    { zone: group(), ...sound({ track: { uri: 'x-test:music', queue_position: 0 } }) }];
  for (const value of invalid) {
    const f = fixture(); await f.m.refresh(); f.handle(() => value);
    assert.equal(await f.m.read('Study@home'), false);
    assert.equal(f.m.state.current, null);
    assert.equal(f.m.review(), false);
  }
});

test('a later room read wins over a slow earlier response', async () => {
  const f = fixture(); await f.m.refresh(); const old = deferred();
  f.handle((route, args) => args.params.room === 'Study@home' ? old.promise : { zone: zones()[1], ...sound({ volume: 30 }) });
  const first = f.m.read('Study@home');
  assert.equal(await f.m.read('Kitchen@other'), true);
  old.resolve({ zone: group(), ...sound() });
  assert.equal(await first, false);
  assert.equal(f.m.state.target, 'Kitchen@other');
  assert.equal(f.m.state.current.volume, 30);
});

test('refresh and close invalidate pending reviews and late reads', async () => {
  const f = fixture(), m = await f.ready(); m.review(); const reviewed = m.state.pending;
  await m.refresh(); assert.equal(await m.confirm(reviewed), false);
  assert.equal(m.state.current, null);
  const old = deferred(); f.handle(() => old.promise);
  const reading = m.read(); m.close(); old.resolve({ zone: group(), ...sound() });
  assert.equal(await reading, false);
  assert.equal(m.state.current, null);
  assert.equal(m.state.loading, false);
  assert.equal(await m.read(), false);
});

test('closing during either preflight prevents every mutation', async () => {
  for (const stage of ['zones', 'fadeState']) {
    const f = fixture(), m = await f.ready(), old = deferred(); m.review();
    f.handle((route) => route === stage ? old.promise : zones());
    const operation = m.confirm(); await tick(); m.close();
    old.resolve(stage === 'zones' ? zones() : { zone: group(), ...sound() });
    assert.equal(await operation, false);
    assert.equal(f.writes().length, 0);
    assert.equal(m.state.writing, false);
  }
});

test('even a synchronous close at the dispatch publication prevents the command', async () => {
  const calls = []; let m;
  m = Fades.model({ call: async (route) => {
    calls.push(route); return route === 'zones' ? zones() : { zone: group(), ...sound() };
  }, changed: (state) => { if (state.message.startsWith('Fading for')) m.close(); } });
  await m.refresh(); await m.read('Study@home'); m.edit('level', '25'); m.review();
  assert.equal(await m.confirm(), false);
  assert.deepEqual(calls, ['zones', 'fadeState', 'zones', 'fadeState']);
});

test('one write stays serialized through double confirmation, close and reopening', async () => {
  const f = fixture(), m = await f.ready(), old = deferred(); m.review(); const reviewed = m.state.pending;
  f.handle((route) => route === 'zones' ? zones() : route === 'fadeState' ? { zone: group(), ...sound() } : old.promise);
  const operation = m.confirm(reviewed); await tick();
  assert.equal(f.writes().length, 1);
  assert.equal(await m.confirm(reviewed), false);
  assert.equal(await m.read(), false);
  assert.equal(m.edit('level', '30'), false);
  assert.equal(m.mode('pause'), false);
  m.close(); assert.equal(await m.refresh(), false);
  assert.equal(m.state.open, true);
  assert.equal(await m.confirm(reviewed), false);
  old.resolve({ done: 'Fade completed.', changed: true });
  assert.equal(await operation, true);
  assert.equal(m.state.current, null, 'an old acknowledgement cannot repaint a reopened panel');
  assert.equal(m.state.writing, false);
  assert.equal(f.writes().length, 1);
});

test('policy-clamped or superseded outcomes are shown without trying to force the requested level', async () => {
  const f = fixture(), m = await f.ready(); m.review();
  f.handle((route) => route === 'zones' ? zones() : route === 'fadeState' ? { zone: group(), ...sound() }
    : { done: 'Capped at 20.', changed: true, volume: 20, notes: [{ code: 'VOLUME_CLAMPED', detail: 'House cap applied.' }] });
  assert.equal(await m.confirm(), true);
  assert.match(m.state.message, /House cap applied/);
  assert.equal(f.writes().length, 1);
  assert.equal(f.calls.length, 5, 'no client-side follow-up ramp or read');
});

test('unchanged is a legitimate server outcome, not grounds for a second command', async () => {
  const f = fixture(), m = await f.ready(); m.review();
  f.handle((route) => route === 'zones' ? zones() : route === 'fadeState' ? { zone: group(), ...sound() }
    : { done: 'Nothing needed doing.', changed: false });
  assert.equal(await m.confirm(), true);
  assert.match(m.state.message, /Nothing needed doing/);
  assert.equal(f.writes().length, 1);
});

test('lost, rejected and malformed replies remain uncertain and are never retried', async () => {
  for (const value of [new Error('Connection lost.'), null, {}, { done: 'done' },
    { done: '', changed: true }, { done: 'done', changed: 1 }, { done: 'done', changed: true, volume: 101 },
    { done: 'done', changed: true, notes: null }, { done: 'done', changed: true, notes: [{}] }]) {
    const f = fixture(), m = await f.ready(); m.review(); const pending = m.state.pending;
    f.handle((route) => {
      if (route === 'zones') return zones();
      if (route === 'fadeState') return { zone: group(), ...sound() };
      if (value instanceof Error) throw value;
      return value;
    });
    assert.equal(await m.confirm(pending), false);
    assert.equal(f.writes().length, 1);
    assert.equal(m.state.current, null);
    assert.equal(m.state.pending, null);
    assert.equal(m.state.writing, false);
    assert.match(m.state.message, /partially completed or may still be running/);
    assert.match(m.state.message, /not repeated/);
    assert.equal(await m.confirm(pending), false);
    assert.equal(f.calls.length, 5);
  }
});

test('preflight failures retain their actionable hint and make no completion claim', async () => {
  const f = fixture(), m = await f.ready(); m.review();
  f.handle(() => { throw Object.assign(new Error('Cannot read.'), { hint: 'Check connectivity.' }); });
  assert.equal(await m.confirm(), false);
  assert.match(m.state.message, /Check connectivity/);
  assert.match(m.state.message, /No fade was sent/);
  assert.equal(f.writes().length, 0);
});

test('disposal cannot be undone by later reads or responses', async () => {
  const f = fixture(), m = await f.ready(), old = deferred(); m.review();
  f.handle((route) => route === 'zones' ? zones() : route === 'fadeState' ? { zone: group(), ...sound() } : old.promise);
  const operation = m.confirm(); await tick(); m.dispose(); const count = f.publications();
  old.resolve({ done: 'Done.', changed: true }); await operation;
  assert.equal(f.publications(), count);
  assert.equal(m.state.current, null);
  assert.equal(await m.refresh(), false);
  assert.equal(await m.read(), false);
  assert.equal(await m.confirm(), false);
});

test('range edits can preserve the active DOM control and send no commands', async () => {
  const f = fixture(), m = await f.ready(), count = f.publications();
  for (const value of ['21', '22', '23']) assert.equal(m.edit('level', value, false), true);
  assert.equal(f.publications(), count);
  assert.equal(m.state.level, '23');
  assert.equal(f.writes().length, 0);
});

test('routes match the native endpoints; the module has no timer-driven writes or embedded secrets', () => {
  assert.deepEqual(Fades.routes, { fadeState: ['GET', '/zones/{room}/state'],
    fadeVolume: ['POST', '/volume/fade'], fadePause: ['POST', '/pause/fade'] });
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/fades.js'), 'utf8');
  assert.doesNotMatch(source, /setTimeout|setInterval|innerHTML|outerHTML|insertAdjacentHTML/);
  for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
    assert.equal(source.toLowerCase().includes(secret), false, secret);
  }
});
