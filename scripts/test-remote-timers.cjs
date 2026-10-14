'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { model, roomsOf, durationOf } = require('../crates/fsonos-api/src/remote/timers.js');
const zones = () => [
  { household: 'S1', coordinator_room: 'Den', members: ['Den', 'Study'] },
  { household: 'S2', coordinator_room: 'Den', members: ['Den'] },
];
const timer = (fields = {}) => ({ zone: 'Den', done: 'Timer updated', changed: false,
  ends: '2026-10-14T12:00:00-04:00', remaining_secs: 1800, fades: true, fading: false, ...fields });
const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
function fixture() {
  const f = { rooms: zones(), timer: [], writes: [], reads: [], write: null };
  f.call = async (route, args) => {
    f.reads.push([route, args]);
    if (route === 'zones') return structuredClone(f.rooms);
    if (route === 'sleepRead') return structuredClone(f.timer);
    assert.equal(route, 'sleepWrite');
    f.writes.push(structuredClone(args.body));
    if (f.write) return f.write(args.body);
    return args.body.cancel ? timer({ ends: null, remaining_secs: null, changed: true }) : timer({ changed: true });
  };
  f.m = model({ call: (...args) => f.call(...args) });
  return f;
}
async function ready(f, active = false) {
  if (active) f.timer = [timer()];
  await f.m.refresh();
  assert.equal(await f.m.target('Den@S2'), true);
  assert.equal(f.m.state.loaded, true);
}

test('nothing is read until opened; households with duplicate names stay distinct', async () => {
  const f = fixture();
  assert.equal(f.reads.length, 0);
  await ready(f);
  assert.deepEqual(f.m.state.rooms.map((r) => r.target), ['Den@S1', 'Den@S2']);
  assert.equal(f.reads.find(([r]) => r === 'sleepRead')[1].query.zone, 'Den@S2');
});
test('set forwards canonical duration once and requires a new read afterwards', async () => {
  const f = fixture(); await ready(f);
  f.m.duration('1H 30M'); assert.equal(f.m.review('set'), true);
  assert.equal(await f.m.confirm(), true);
  assert.deepEqual(f.writes, [{ zone: 'Den@S2', duration: '1h30m' }]);
  assert.equal(f.m.state.loaded, false);
  assert.equal(f.m.review('set'), false);
});
test('extend and cancel use their distinct shared API shapes', async () => {
  for (const action of ['extend', 'cancel']) {
    const f = fixture(); await ready(f, true);
    assert.equal(f.m.review(action), true);
    assert.equal(await f.m.confirm(), true);
    assert.deepEqual(f.writes[0], action === 'cancel'
      ? { zone: 'Den@S2', cancel: true } : { zone: 'Den@S2', duration: '45m', extend: true });
  }
});
test('empty timers cannot be extended or cancelled', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.m.review('extend'), false); assert.equal(f.m.review('cancel'), false);
  assert.equal(await f.m.confirm(), false); assert.equal(f.writes.length, 0);
});
test('duration validation has the same 23-hour ceiling and refuses overflow', () => {
  for (const s of ['45m', '1h30m', '90s', '45', '23h', '1h 30m', '1H']) assert.ok(durationOf(s), s);
  for (const s of ['', '0', '-1', '24h', '23h1s', '1.5h', '1d', 'NaN', '9'.repeat(80), '2h junk']) assert.equal(durationOf(s), null, s);
});
test('editing duration or cancelling invalidates the reviewed request', async () => {
  const f = fixture(); await ready(f);
  f.m.review('set'); f.m.duration('5m'); assert.equal(await f.m.confirm(), false);
  f.m.review('set'); f.m.cancel(); assert.equal(await f.m.confirm(), false);
  assert.equal(f.writes.length, 0);
});
test('a changed group at confirmation issues no timer mutation', async () => {
  const f = fixture(); await ready(f); f.m.review('set'); f.rooms[1].members.push('New room');
  assert.equal(await f.m.confirm(), false); assert.equal(f.writes.length, 0);
  assert.match(f.m.state.message, /group changed/);
});
test('a newly installed, replaced, vanished or fading timer invalidates review', async () => {
  for (const [old, fresh] of [[[], [timer()]], [[timer()], []], [[timer()], [timer({ ends: '2026-10-14T13:00:00-04:00' })]], [[timer()], [timer({ fading: true })]]]) {
    const f = fixture(); f.timer = old; await ready(f); f.m.review('set'); f.timer = fresh;
    assert.equal(await f.m.confirm(), false); assert.equal(f.writes.length, 0);
  }
});
test('normal countdown and speaker second-rounding do not invalidate review', async () => {
  const f = fixture(); f.timer = [timer({ fades: false })]; await ready(f); f.m.review('extend');
  f.timer = [timer({ remaining_secs: 1200, fades: false, ends: '2026-10-14T12:00:01-04:00' })];
  assert.equal(await f.m.confirm(), true); assert.equal(f.writes.length, 1);
});
test('duplicate confirms and reads during a pending write cannot repeat it', async () => {
  const f = fixture(); await ready(f); const wait = deferred(); f.write = () => wait.promise;
  f.m.review('set'); const first = f.m.confirm();
  assert.equal(await f.m.confirm(), false); assert.equal(await f.m.refresh(), false);
  wait.resolve(timer({ changed: true })); assert.equal(await first, true); assert.equal(f.writes.length, 1);
});
test('closing or disposing during preflight prevents the pending write', async () => {
  for (const action of ['close', 'dispose']) {
    const f = fixture(); await ready(f); const wait = deferred(); const original = f.call;
    f.call = (route, args) => route === 'zones' ? wait.promise : original(route, args);
    f.m.review('set'); const result = f.m.confirm(); f.m[action](); wait.resolve(zones());
    assert.equal(await result, false); assert.equal(f.writes.length, 0);
  }
});
test('lost and malformed replies invalidate state and never repeat the mutation', async () => {
  for (const write of [() => { throw new Error('lost reply'); }, () => null,
    () => timer({ zone: 'Other' }), () => timer({ ends: null, remaining_secs: null })]) {
    const f = fixture(); await ready(f); f.write = write; f.m.review('set');
    assert.equal(await f.m.confirm(), false); assert.equal(await f.m.confirm(), false);
    assert.equal(f.writes.length, 1); assert.equal(f.m.state.loaded, false);
    assert.match(f.m.state.message, /not repeated/);
  }
});
test('unreadable timer data never becomes permission to replace a timer', async () => {
  for (const list of [null, [timer(), timer()], [timer({ ends: 'tomorrow' })], [timer({ zone: 'Other' })], [timer({ remaining_secs: -1 })]]) {
    const f = fixture(); f.timer = list; await f.m.refresh(); assert.equal(await f.m.target('Den@S2'), false);
    assert.equal(f.m.review('set'), false); assert.equal(f.writes.length, 0);
  }
});
test('newer group reads supersede late older responses', async () => {
  const f = fixture(); const wait = deferred(); const original = f.call; let first = true;
  f.call = (route, args) => route === 'zones' && first ? (first = false, wait.promise) : original(route, args);
  const stale = f.m.refresh(); await f.m.refresh();
  wait.resolve([{ household: 'S1', coordinator_room: 'Old', members: ['Old'] }]);
  assert.equal(await stale, false); assert.equal(f.m.state.rooms[0].target, 'Den@S1');
});
test('room observations reject ambiguous or malformed memberships', () => {
  for (const value of [null, [{}], [{ household: 'S1', coordinator_room: 'A', members: ['B'] }],
    [{ household: 'S1', coordinator_room: 'A', members: ['A', 'A'] }], [...zones(), ...zones()]]) assert.throws(() => roomsOf(value));
});
test('regrouping during a timer read invalidates the resulting observation', async () => {
  const f = fixture(); await ready(f); const original = f.call;
  f.call = async (route, args) => { const v = await original(route, args); if (route === 'sleepRead') f.rooms[1].members.push('New room'); return v; };
  assert.equal(await f.m.refresh(), false); assert.equal(f.m.state.loaded, false);
});
