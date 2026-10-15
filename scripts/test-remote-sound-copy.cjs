// Run: node --test scripts/test-remote-sound-copy.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Copy = require('../crates/fsonos-api/src/remote/sound-copy.js');
const wanted = { bass: 4, treble: -2, loudness: false };
const flat = { bass: 0, treble: 0, loudness: true };
const zones = [{ household: 'S2', coordinator_room: 'First', members: ['First', 'Second', 'Third'] },
  { household: 'S1', coordinator_room: 'First', members: ['First'] }];
const tick = () => new Promise((r) => setImmediate(r));
function deferred() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
function fixture(options = {}) {
  let groups = options.zones || zones, hook = null;
  const tones = new Map(Copy.inventory(groups).map((r) => [r.target, { ...flat }]));
  tones.set('First@S2', { ...wanted });
  const calls = [];
  const normal = async (name, args) => {
    if (name === 'zones') return structuredClone(groups);
    const target = args.params?.room || args.body.zone;
    assert.ok(tones.has(target), target);
    if (name === 'soundCopyWrite') {
      const next = { ...tones.get(target) };
      for (const field of ['bass', 'treble', 'loudness']) if (field in args.body) next[field] = args.body[field];
      tones.set(target, next);
      return { done: 'Changed.', eq: { room: target.split('@')[0], ...next } };
    }
    assert.equal(name, 'soundCopyRead');
    return { room: target.split('@')[0], ...tones.get(target) };
  };
  const call = async (name, args) => { calls.push({ name, args: structuredClone(args) }); return hook ? hook(name, args, normal) : normal(name, args); };
  const model = Copy.model({ call, changed: options.changed });
  return { model, calls, tones, normal, call,
    hook: (f) => { hook = f; }, groups: (v) => { groups = v; },
    writes: () => calls.filter((c) => c.name === 'soundCopyWrite'),
    async ready(targets = ['Second@S2', 'Third@S2']) {
      await model.refresh(); model.selectSource('First@S2'); await model.readSource();
      for (const target of targets) assert.equal(model.selectTarget(target, true), true);
      const plan = await model.prepare(); assert.ok(plan); return plan;
    } };
}

test('inventory includes noncoordinators and separates the same name across households', () => {
  assert.deepEqual(Copy.inventory(zones).map((r) => r.target), ['First@S2', 'Second@S2', 'Third@S2', 'First@S1']);
  assert.ok(Object.isFrozen(Copy.inventory(zones)[0]));
});
test('ambiguous, duplicated and malformed inventories fail closed', () => {
  for (const value of [null, {}, [null], [{ ...zones[0], members: [] }],
    [{ ...zones[0], members: ['Second'] }], [{ ...zones[0], members: ['First', 'First'] }],
    [zones[0], zones[0]], [{ household: 'b@c', coordinator_room: 'a', members: ['a'] },
      { household: 'c', coordinator_room: 'a@b', members: ['a@b'] }]]) assert.throws(() => Copy.inventory(value));
});
test('tone reads require the correct room, bounded integer levels and boolean loudness', () => {
  const room = Copy.inventory(zones)[0];
  for (const value of [null, {}, { room: 'Second', ...flat }, { room: 'First', ...flat, bass: 11 },
    { room: 'First', ...flat, treble: -11 }, { room: 'First', ...flat, bass: '0' },
    { room: 'First', ...flat, bass: 0.5 }, { room: 'First', ...flat, loudness: 0 }]) {
    assert.throws(() => Copy.observed(value, room));
  }
});
test('nothing reads or selects speakers before opening, and no write happens while preparing', async () => {
  const f = fixture(); assert.deepEqual(f.calls, []);
  assert.equal(await f.model.prepare(), null); assert.equal(await f.model.confirm({}), false);
  const plan = await f.ready(); assert.equal(f.writes().length, 0);
  assert.deepEqual(plan.tone, wanted); assert.equal(plan.entries.length, 2);
  assert.ok(Object.isFrozen(plan)); assert.ok(Object.isFrozen(plan.entries));
  assert.throws(() => { plan.entries[0].patch.bass = 9; });
});
test('a complete copy writes each selected room once, reads back, and never writes its source', async () => {
  const f = fixture(); const plan = await f.ready(['Second@S2', 'First@S1']);
  assert.equal(await f.model.confirm(plan), true);
  assert.deepEqual(f.writes().map((c) => c.args.body), [
    { zone: 'Second@S2', ...wanted }, { zone: 'First@S1', ...wanted }]);
  assert.deepEqual(f.tones.get('Third@S2'), flat);
  assert.deepEqual(f.tones.get('First@S2'), wanted);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['verified', 'verified']);
  for (const row of f.model.view().report.rows) assert.deepEqual(row.observed, wanted);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 2);
});
test('patches preserve zero and false and omit destination fields already matching the source', async () => {
  const f = fixture(); f.tones.set('First@S2', { bass: 0, treble: -2, loudness: false });
  f.tones.set('Second@S2', { bass: 5, treble: -2, loudness: true });
  const plan = await f.ready(['Second@S2']); await f.model.confirm(plan);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Second@S2', bass: 0, loudness: false });
});
test('already-matching speakers get no mutation or redundant action-log entry', async () => {
  const f = fixture(); f.tones.set('Second@S2', { ...wanted });
  const plan = await f.ready(); assert.deepEqual(plan.entries[0].patch, {});
  assert.equal(await f.model.confirm(plan), true); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['unchanged', 'verified']);
});
test('selecting a source removes it from destinations and cannot copy to itself', async () => {
  const f = fixture(); await f.ready();
  assert.equal(f.model.selectTarget('First@S2', true), false);
  assert.equal(f.model.selectSource('Second@S2'), true);
  assert.deepEqual(f.model.view().targets, ['Third@S2']);
  assert.equal(f.model.view().sourceTone, null); assert.equal(f.model.view().review, null);
});
test('changing any destination consumes the old review', async () => {
  const f = fixture(); const plan = await f.ready();
  f.model.selectTarget('Third@S2', false);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.equal(await f.model.confirm({ ...await f.model.prepare() }), false);
});
test('an unread or malformed destination prevents a review instead of assuming defaults', async () => {
  const f = fixture(); await f.model.refresh(); f.model.selectSource('First@S2'); await f.model.readSource();
  f.model.selectTarget('Second@S2', true);
  f.hook((name, args, normal) => args?.params?.room === 'Second@S2' ? { room: 'Second', bass: 0 } : normal(name, args));
  assert.equal(await f.model.prepare(), null); assert.equal(f.model.view().canPrepare, false);
  assert.equal(f.writes().length, 0); assert.match(f.model.view().detail, /Second/);
});
test('source drift after review prevents every write', async () => {
  const f = fixture(); const plan = await f.ready(); f.tones.get('First@S2').bass = 7;
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.match(f.model.view().detail, /source sound changed/);
  assert.ok(f.model.view().report.rows.every((r) => r.status === 'not_attempted'));
});
test('all destinations are preflighted before the first write, even if the last one drifted', async () => {
  const f = fixture(); const plan = await f.ready(); f.tones.get('Third@S2').bass = 8;
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['not_attempted', 'conflict']);
});
test('a disappearing speaker stops the entire copy before mutation', async () => {
  const f = fixture(); const plan = await f.ready(); f.groups([{ ...zones[0], members: ['First', 'Second'] }]);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.match(f.model.view().detail, /Third.*no longer available/);
});
test('regrouping does not confuse per-room sound with the new coordinator', async () => {
  const f = fixture(); const plan = await f.ready();
  f.groups([{ ...zones[0], coordinator_room: 'Third' }, zones[1]]);
  assert.equal(await f.model.confirm(plan), true);
  assert.deepEqual(f.writes().map((c) => c.args.body.zone), ['Second@S2', 'Third@S2']);
});
test('a downstream change after the first command stops remaining writes without rollback', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const answer = await normal(name, args);
    if (name === 'soundCopyWrite') f.tones.get('Third@S2').treble = 9;
    return answer;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['verified', 'conflict']);
  assert.deepEqual(f.tones.get('Second@S2'), wanted);
});
test('lost replies stop immediately, preserve per-room uncertainty, and never automatically retry', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const value = await normal(name, args);
    if (name === 'soundCopyWrite') throw new Error('connection lost');
    return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['uncertain', 'not_attempted']);
  assert.equal(await f.model.confirm(plan), false);
  f.model.close(); f.model.resume(); await f.model.refresh(); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'uncertain');
  f.hook(null); f.model.selectSource('First@S2'); await f.model.readSource();
  const next = await f.model.prepare(); assert.ok(next);
  assert.equal(await f.model.confirm(next), true);
  assert.deepEqual(f.writes().map((c) => c.args.body.zone), ['Second@S2', 'Third@S2']);
});
test('an accepted command without readable verification does not allow the next speaker to change', async () => {
  const f = fixture(); const plan = await f.ready(); let sent = false;
  f.hook(async (name, args, normal) => {
    if (sent && name === 'soundCopyRead') throw new Error('read-back unavailable');
    const value = await normal(name, args); if (name === 'soundCopyWrite') sent = true; return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'accepted_unverified');
  assert.equal(f.model.view().report.rows[0].observed, null);
});
test('a successful acknowledgement with different actual settings is not presented as verified', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const value = await normal(name, args);
    if (name === 'soundCopyWrite') f.tones.set(args.body.zone, { ...flat });
    return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  const row = f.model.view().report.rows[0]; assert.equal(row.status, 'different'); assert.deepEqual(row.observed, flat);
});
test('a malformed acknowledgement is uncertain even when the request took effect', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => { const value = await normal(name, args); return name === 'soundCopyWrite' ? {} : value; });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'uncertain');
});
test('double confirmation and edits are locked until the single in-flight command settles', async () => {
  const f = fixture(); const plan = await f.ready(); const gate = deferred();
  f.hook(async (name, args, normal) => name === 'soundCopyWrite' ? gate.promise : normal(name, args));
  const first = f.model.confirm(plan); await tick();
  assert.equal(f.writes().length, 1); assert.equal(await f.model.confirm(plan), false);
  assert.equal(f.model.selectTarget('First@S1', true), false); assert.equal(f.model.selectSource('Second@S2'), false);
  assert.equal(await f.model.readSource(), false); assert.equal(await f.model.refresh(), false);
  f.model.cancel(); gate.resolve({ done: 'accepted', eq: { room: 'Second', ...wanted } }); await first;
  assert.equal(f.writes().length, 1); assert.equal(f.model.view().writing, false);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['accepted_unverified', 'not_attempted']);
});
for (const transition of ['cancel', 'close', 'suspend', 'dispose']) {
  test(`${transition} during preflight stops before any dispatch`, async () => {
    const f = fixture(); const plan = await f.ready(); const gate = deferred();
    f.hook((name, args, normal) => name === 'zones' ? gate.promise : normal(name, args));
    const pending = f.model.confirm(plan); f.model[transition](); gate.resolve(zones); await pending;
    assert.equal(f.writes().length, 0); assert.equal(f.model.view().writing, false);
  });
}
test('a cancellation requested from a progress callback is checked before dispatch', async () => {
  let model;
  const f = fixture({ changed: (state) => { if (state.report?.rows.some((r) => r.status === 'sending')) model.cancel(); } });
  model = f.model; const plan = await f.ready(); await model.confirm(plan);
  assert.equal(f.writes().length, 0); assert.equal(model.view().report.rows[0].status, 'not_sent');
});
test('view errors cannot turn accepted commands into retries', async () => {
  const f = fixture({ changed: () => { throw new Error('view detached'); } });
  assert.equal(await f.model.confirm(await f.ready()), true); assert.equal(f.writes().length, 2);
});
test('a stale source read cannot populate a newly selected source', async () => {
  const f = fixture(); await f.model.refresh(); f.model.selectSource('First@S2'); const gate = deferred();
  f.hook((name, args, normal) => args?.params?.room === 'First@S2' ? gate.promise : normal(name, args));
  const old = f.model.readSource(); f.model.selectSource('First@S1'); await f.model.readSource();
  gate.resolve({ room: 'First', ...wanted }); await old;
  assert.deepEqual(f.model.view().sourceTone, flat); assert.equal(f.model.view().source, 'First@S1');
});
test('destination count and read concurrency are bounded', async () => {
  const many = [{ household: 'S2', coordinator_room: 'First', members: ['First', ...Array.from({ length: 34 }, (_, i) => `Room${i}`)] }];
  const f = fixture({ zones: many }); await f.model.refresh(); f.model.selectSource('First@S2'); await f.model.readSource();
  for (let i = 0; i < Copy.MAX_TARGETS; ++i) assert.equal(f.model.selectTarget(`Room${i}@S2`, true), true);
  assert.equal(f.model.selectTarget('Room33@S2', true), false);
  let running = 0, peak = 0;
  f.hook(async (name, args, normal) => {
    if (name !== 'soundCopyRead') return normal(name, args);
    peak = Math.max(peak, ++running); await tick(); const value = await normal(name, args); --running; return value;
  });
  assert.ok(await f.model.prepare()); assert.equal(peak, Copy.READ_CONCURRENCY); assert.equal(running, 0);
  assert.equal(f.writes().length, 0);
});
