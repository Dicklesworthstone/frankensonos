'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const Groups = require('../crates/fsonos-api/src/remote/groups.js');

const group = (household, coordinator, ...members) => ({ household,
  coordinator_room: coordinator, members: [coordinator, ...members], degraded: [], transport_state: 'playing' });
const house = () => [group('S1', 'Den', 'Study'), group('S1', 'Kitchen'), group('S2', 'Den'), group('S2', 'Hall')];
const ok = () => ({ done: 'group change complete', changed: true, notes: [] });
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
function fixture() {
  const calls = [];
  let zones = house();
  let answer = ok();
  let nextRead = null;
  let updates = 0;
  const model = Groups.model({ call: async (route, args) => {
    calls.push([route, args]);
    if (route === 'zones') {
      if (nextRead) { const read = nextRead; nextRead = null; return read(); }
      return zones;
    }
    return typeof answer === 'function' ? answer(route, args) : answer;
  }, updated: () => { updates += 1; } });
  return { model, calls, setZones(value) { zones = value; }, answer(value) { answer = value; },
    nextRead(value) { nextRead = value; }, writes: () => calls.filter(([route]) => route !== 'zones'),
    updates: () => updates };
}
async function ready(f, from = 'Study@S1', to = 'Kitchen@S1') {
  assert.equal(await f.model.refresh(), true);
  assert.equal(f.model.choose('source', from), true);
  assert.equal(f.model.choose('destination', to), true);
}

test('the panel model is lazy and unknown selections cannot start work', () => {
  const f = fixture();
  assert.equal(f.model.review('join'), false);
  assert.equal(f.model.choose('source', 'Study@S1'), false);
  assert.deepEqual(f.calls, []);
});

test('join sends exactly one household-qualified request after a fresh read', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.model.review('join'), true);
  assert.equal(await f.model.confirm(), true);
  assert.deepEqual(f.writes(), [['groupsJoin', { body: { zone: 'Study@S1', to: 'Kitchen@S1' } }]]);
  assert.equal(f.calls[1][0], 'zones');
  assert.equal(f.updates(), 1);
});

test('separation uses one room, never a wildcard set', async () => {
  const f = fixture(); await ready(f, 'Den@S1');
  assert.equal(f.model.review('leave'), true);
  assert.match(f.model.state.pending.warning, /coordinator/);
  assert.equal(await f.model.confirm(), true);
  assert.deepEqual(f.writes()[0], ['groupsLeave', { body: { zone: 'Den@S1' } }]);
});

test('whole-house grouping names the selected group coordinator and lists only its household', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.model.review('party'), true);
  assert.equal(f.model.state.pending.labels.length, 2);
  assert.equal(await f.model.confirm(), true);
  assert.deepEqual(f.writes()[0], ['groupsParty', { body: { zone: 'Den@S1' } }]);
});

test('duplicate room names across households remain distinct and cross-household joins fail', async () => {
  const f = fixture(); await ready(f, 'Den@S1', 'Den@S2');
  assert.equal(f.model.review('join'), false);
  assert.match(f.model.state.message, /Different households/);
  assert.equal(await f.model.confirm(), false);
  assert.deepEqual(f.writes(), []);
});

test('already-grouped joins and standalone separation are no-ops without dispatch', async () => {
  const f = fixture(); await ready(f, 'Den@S1', 'Study@S1');
  assert.equal(f.model.review('join'), false);
  f.model.choose('source', 'Kitchen@S1');
  assert.equal(f.model.review('leave'), false);
  assert.deepEqual(f.writes(), []);
});

test('cancel and selection changes discard the frozen review', async () => {
  const f = fixture(); await ready(f);
  f.model.review('join');
  assert.equal(Object.isFrozen(f.model.state.pending.body), true);
  f.model.cancel();
  assert.equal(await f.model.confirm(), false);
  f.model.review('join');
  f.model.choose('source', 'Den@S2');
  assert.equal(await f.model.confirm(), false);
  assert.deepEqual(f.writes(), []);
});

test('changed source or destination membership refuses a stale review', async () => {
  for (const replacement of [
    [group('S1', 'Den'), group('S1', 'Study'), group('S1', 'Kitchen')],
    [group('S1', 'Den', 'Study'), group('S1', 'Kitchen', 'Guest')],
    [group('S1', 'Study', 'Den'), group('S1', 'Kitchen')],
  ]) {
    const f = fixture(); await ready(f); f.model.review('join');
    f.setZones(replacement);
    assert.equal(await f.model.confirm(), false);
    assert.match(f.model.state.message, /No group change was sent/);
    assert.deepEqual(f.writes(), []);
    assert.equal(f.model.review('join'), false);
  }
});

test('group and member ordering changes do not invalidate membership', async () => {
  const f = fixture(); await ready(f); f.model.review('join');
  const zones = house().reverse(); zones[3].members.reverse(); f.setZones(zones);
  assert.equal(await f.model.confirm(), true);
});

test('unrelated household changes do not cancel a local join', async () => {
  const f = fixture(); await ready(f); f.model.review('join');
  f.setZones([...house().slice(0, 2), group('S2', 'Hall', 'Den')]);
  assert.equal(await f.model.confirm(), true);
});

test('whole-house review rejects a newly discovered room before dispatch', async () => {
  const f = fixture(); await ready(f); f.model.review('party');
  f.setZones([...house(), group('S1', 'Guest')]);
  assert.equal(await f.model.confirm(), false);
  assert.deepEqual(f.writes(), []);
});

test('partial and ambiguous topology responses cannot authorize grouping', async () => {
  for (const zones of [null, {}, [group('S1', 'Den'), group('S1', 'Den')],
    [{ ...group('S1', 'Den'), members: [] }], [{ ...group('S1', 'Den'), members: ['Study'] }],
    [{ ...group('S1', 'Den'), degraded: ['Ghost'] }]]) {
    const f = fixture(); f.setZones(zones);
    assert.equal(await f.model.refresh(), false);
    assert.equal(f.model.review('party'), false);
    assert.deepEqual(f.writes(), []);
  }
});

test('late reads cannot replace a newer topology view', async () => {
  const f = fixture(); const old = deferred(); f.nextRead(() => old.promise);
  const first = f.model.refresh();
  f.setZones([group('S2', 'New room')]); await f.model.refresh();
  old.resolve(house()); assert.equal(await first, false);
  assert.equal(f.model.state.groups[0].coordinator, 'New room');
});

test('closing or disposing during preflight prevents the write', async () => {
  for (const close of ['close', 'dispose']) {
    const f = fixture(); await ready(f); f.model.review('join');
    const gate = deferred(); f.nextRead(() => gate.promise);
    const pending = f.model.confirm(); f.model[close]();
    gate.resolve(house()); assert.equal(await pending, false);
    assert.deepEqual(f.writes(), []);
    assert.equal(f.model.state.writing, false);
  }
});

test('double confirmation and refresh during a write never duplicate a mutation', async () => {
  const f = fixture(); await ready(f); f.model.review('join');
  const gate = deferred(); f.answer(() => gate.promise);
  const pending = f.model.confirm(); await Promise.resolve(); await Promise.resolve();
  assert.equal(await f.model.confirm(), false);
  assert.equal(await f.model.refresh(), false);
  assert.equal(f.model.choose('source', 'Den@S2'), false);
  gate.resolve(ok()); assert.equal(await pending, true);
  assert.equal(f.writes().length, 1);
});

test('lost and malformed replies require fresh reads and are never replayed', async () => {
  for (const answer of [null, {}, { done: 'maybe' }, { done: 'x', changed: true, notes: [{}] },
    () => { throw new Error('lost reply'); }]) {
    const f = fixture(); await ready(f); f.model.review('join'); f.answer(answer);
    assert.equal(await f.model.confirm(), false);
    assert.equal(f.writes().length, 1);
    assert.equal(f.model.review('join'), false);
    assert.equal(await f.model.confirm(), false);
    assert.match(f.model.state.message, /not repeated/);
    assert.equal(f.calls.filter(([route]) => route === 'zones').length, 2);
  }
});

test('read failures do not send writes and preserve the server hint', async () => {
  const f = fixture(); await ready(f); f.model.review('join');
  f.nextRead(() => { throw Object.assign(new Error('unreachable'), { hint: 'Check the speaker.' }); });
  assert.equal(await f.model.confirm(), false);
  assert.match(f.model.state.message, /Check the speaker/);
  assert.deepEqual(f.writes(), []);
});

test('policy notes and partial-result details remain visible after success', async () => {
  const f = fixture(); await ready(f); f.model.review('join');
  f.answer({ done: 'one room joined; another unavailable', changed: true,
    notes: [{ code: 'VOLUME_CLAMPED', detail: 'Room volume lowered first.' }] });
  assert.equal(await f.model.confirm(), true);
  assert.match(f.model.state.message, /another unavailable/);
  assert.match(f.model.state.message, /lowered first/);
});
