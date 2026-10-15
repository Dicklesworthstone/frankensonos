'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const Scenes = require('../crates/fsonos-api/src/remote/scenes.js');

const row = (extra = {}) => ({
  name: 'Evening',
  saved: 1_900_000_000,
  groups: [{ coordinator: 'Living Room@S2', members: ['Kitchen@S2'],
    source: { kind: 'favorite', name: 'Dinner Mix' }, playing: true }],
  volumes: { 'Living Room@S2': 28, 'Kitchen@S2': 24 },
  mutes: { 'Living Room@S2': false, 'Kitchen@S2': false },
  ...extra,
});
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((ok, no) => { resolve = ok; reject = no; });
  return { promise, resolve, reject };
};

function fixture(initial = [row()]) {
  let rows = structuredClone(initial);
  const writes = [];
  let reads = 0;
  const call = async (name, options = {}) => {
    if (name === 'scenes') { reads += 1; return structuredClone(rows); }
    if (name === 'scene') {
      const found = rows.find((r) => r.name === options.params.name);
      if (!found) throw new Error('missing');
      return structuredClone(found);
    }
    writes.push({ name, ...structuredClone(options) });
    if (name === 'sceneSave') {
      const saved = row({ name: options.params.name, saved: row().saved + 1 });
      rows = rows.filter((r) => r.name !== saved.name).concat(saved);
      return structuredClone(saved);
    }
    if (name === 'sceneDelete') {
      rows = rows.filter((r) => r.name !== options.params.name);
      return { deleted: options.params.name, done: 'deleted' };
    }
    if (name === 'sceneApplyReviewed') {
      return { scene: options.params.name, done: 'applied', changed: true, complete: true,
        steps: ['Living Room plays'], failed: [], skipped: [], notes: [] };
    }
    throw new Error(name);
  };
  return { control: Scenes.model({ call }), writes, reads: () => reads,
    set: (value) => { rows = structuredClone(value); } };
}

test('scene decoding freezes a complete definition and rejects malformed house state', () => {
  const scene = Scenes.sceneOf(row());
  assert.equal(Object.isFrozen(scene), true);
  assert.equal(Object.isFrozen(scene.groups), true);
  assert.equal(scene.groups[0].source.name, 'Dinner Mix');
  for (const bad of [
    row({ name: '' }),
    row({ saved: 1.5 }),
    row({ groups: [{ coordinator: '', members: [], source: { kind: 'keep' } }] }),
    row({ groups: [{ coordinator: 'A', members: ['A'], source: { kind: 'keep' } }] }),
    row({ groups: [{ coordinator: 'A', members: [], source: { kind: 'bogus' } }] }),
    row({ volumes: { A: 101 } }),
    row({ mutes: { A: 'false' } }),
  ]) assert.throws(() => Scenes.sceneOf(bad));
  assert.throws(() => Scenes.scenesOf([row(), row()]), /duplicate/);
});

test('signature ignores save time but binds every operational field', () => {
  const a = Scenes.sceneOf(row());
  assert.equal(Scenes.signature(a), Scenes.signature(Scenes.sceneOf(row({ saved: a.saved + 10 }))));
  assert.notEqual(Scenes.signature(a), Scenes.signature(Scenes.sceneOf(row({
    volumes: { ...row().volumes, 'Kitchen@S2': 25 },
  }))));
});

test('opening, reviewing and cancelling are read-only', async () => {
  const f = fixture();
  assert.equal(await f.control.refresh(), true);
  assert.equal(f.control.review('apply', 'Evening'), true);
  f.control.cancel();
  assert.equal(await f.control.confirm(null), false);
  assert.deepEqual(f.writes, []);
});

test('apply rechecks list and exact scene, then sends one reviewed fade', async () => {
  const f = fixture();
  await f.control.refresh();
  f.control.editFade('12');
  assert.equal(f.control.review('apply', 'Evening'), true);
  const p = f.control.state.pending;
  assert.equal(await f.control.confirm(p), true);
  assert.deepEqual(f.writes, [{
    name: 'sceneApplyReviewed',
    params: { name: 'Evening' },
    body: { fade_secs: 12 },
  }]);
  assert.equal(f.reads(), 2);
  assert.equal(f.control.state.loaded, false);
});

test('zero fade is represented by an empty apply body', async () => {
  const f = fixture();
  await f.control.refresh();
  f.control.editFade('0');
  f.control.review('apply', 'Evening');
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.deepEqual(f.writes[0].body, {});
});

test('invalid fades never reach the daemon', async () => {
  for (const value of ['-1', '61', '1.5', 'x']) {
    const f = fixture();
    await f.control.refresh();
    f.control.editFade(value);
    assert.equal(f.control.review('apply', 'Evening'), false);
    assert.deepEqual(f.writes, []);
  }
});

test('capture creates a new scene and overwrites only the reviewed existing definition', async () => {
  let f = fixture([]);
  await f.control.refresh();
  f.control.editName('Morning');
  assert.equal(f.control.review('capture'), true);
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.deepEqual(f.writes[0], { name: 'sceneSave', params: { name: 'Morning' }, body: {} });

  f = fixture();
  await f.control.refresh();
  f.control.editName('Evening');
  assert.equal(f.control.review('capture'), true);
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.equal(f.writes.length, 1);
});

test('capture aborts when existence or the overwritten definition changes', async () => {
  const newOne = fixture([]);
  await newOne.control.refresh();
  newOne.control.editName('Evening');
  newOne.control.review('capture');
  const pending = newOne.control.state.pending;
  newOne.set([row()]);
  assert.equal(await newOne.control.confirm(pending), false);
  assert.deepEqual(newOne.writes, []);

  const oldOne = fixture();
  await oldOne.control.refresh();
  oldOne.control.editName('Evening');
  oldOne.control.review('capture');
  const old = oldOne.control.state.pending;
  oldOne.set([row({ volumes: { 'Living Room@S2': 99 } })]);
  assert.equal(await oldOne.control.confirm(old), false);
  assert.deepEqual(oldOne.writes, []);
});

test('delete rechecks the exact definition and sends once', async () => {
  const f = fixture();
  await f.control.refresh();
  f.control.review('delete', 'Evening');
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.deepEqual(f.writes, [{ name: 'sceneDelete', params: { name: 'Evening' }, body: {} }]);
});

test('changed or removed scenes abort apply and delete before any write', async () => {
  for (const action of ['apply', 'delete']) {
    const f = fixture();
    await f.control.refresh();
    f.control.review(action, 'Evening');
    const pending = f.control.state.pending;
    f.set(action === 'apply' ? [row({ groups: [] })] : []);
    assert.equal(await f.control.confirm(pending), false);
    assert.deepEqual(f.writes, []);
    assert.match(f.control.state.message, /changed or was removed/);
  }
});

test('refresh and close invalidate old reviews', async () => {
  const f = fixture();
  await f.control.refresh();
  f.control.review('apply', 'Evening');
  const old = f.control.state.pending;
  await f.control.refresh();
  assert.equal(await f.control.confirm(old), false);
  f.control.review('delete', 'Evening');
  const next = f.control.state.pending;
  f.control.close();
  assert.equal(await f.control.confirm(next), false);
  assert.deepEqual(f.writes, []);
});

test('closing during preflight prevents an unsent mutation', async () => {
  const wait = deferred();
  let sceneReads = 0, writes = 0;
  const control = Scenes.model({ call: async (name) => {
    if (name === 'scenes') return ++sceneReads === 1 ? [row()] : wait.promise;
    if (name === 'scene') return row();
    writes += 1;
    return {};
  } });
  await control.refresh();
  control.review('apply', 'Evening');
  const work = control.confirm(control.state.pending);
  control.close();
  wait.resolve([row()]);
  assert.equal(await work, false);
  assert.equal(writes, 0);
});

test('uncertain mutation replies never replay and require a new refresh', async () => {
  let writes = 0;
  const control = Scenes.model({ call: async (name) => {
    if (name === 'scenes') return [row()];
    if (name === 'scene') return row();
    writes += 1;
    throw new Error('connection closed');
  } });
  await control.refresh();
  control.review('apply', 'Evening');
  const pending = control.state.pending;
  assert.equal(await control.confirm(pending), false);
  assert.equal(await control.confirm(pending), false);
  assert.equal(writes, 1);
  assert.equal(control.review('apply', 'Evening'), false);
  assert.match(control.state.message, /not repeated/);
});

test('partial apply is surfaced as incomplete rather than reported as success', async () => {
  const f = fixture();
  const control = Scenes.model({ call: async (name, options) => {
    if (name === 'scenes') return [row()];
    if (name === 'scene') return row();
    if (name === 'sceneApplyReviewed') {
      f.writes.push({ name, ...options });
      return { scene: 'Evening', done: 'partial', changed: true, complete: false,
        steps: ['one'], failed: [{ step: 'two', error: 'offline' }], skipped: [], notes: [] };
    }
    throw new Error(name);
  } });
  await control.refresh();
  control.review('apply', 'Evening');
  assert.equal(await control.confirm(control.state.pending), false);
  assert.match(control.state.message, /in part/);
  assert.equal(f.writes.length, 1);
});

test('late list reads cannot resurrect a closed panel', async () => {
  const wait = deferred();
  const control = Scenes.model({ call: () => wait.promise });
  const work = control.refresh();
  control.close();
  wait.resolve([row()]);
  assert.equal(await work, false);
  assert.equal(control.state.loaded, false);
});
