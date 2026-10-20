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

// This is the actual successful SceneApplyDto wire shape: serde omits empty
// failed/skipped/notes, rather than sending the arrays in the old fixtures.
const applied = (extra = {}) => ({ scene: 'Evening', done: 'applied scene Evening (1 step(s))',
  changed: true, complete: true, steps: ['Living Room plays'], ...extra });
function responder(reply) {
  const writes = [];
  const control = Scenes.model({ call: async (name, options) => {
    if (name === 'scenes') return [row()];
    if (name === 'scene') return row();
    writes.push({ name, ...options });
    return structuredClone(reply);
  } });
  return { control, writes };
}
async function applyReply(reply) {
  const f = responder(reply);
  await f.control.refresh();
  f.control.review('apply', 'Evening');
  const ok = await f.control.confirm(f.control.state.pending);
  return { ...f, ok };
}

test('production success and no-op payloads with omitted empty lists are acknowledged', async () => {
  for (const reply of [applied(), applied({ changed: false, steps: [], done: 'Already matches' })]) {
    const f = await applyReply(reply);
    assert.equal(f.ok, true);
    assert.equal(f.writes.length, 1);
    assert.equal(f.control.state.result.done, reply.done);
    assert.deepEqual(f.control.state.result.failed, []);
    assert.deepEqual(f.control.state.result.skipped, []);
    assert.deepEqual(f.control.state.result.notes, []);
  }
});

test('partial results retain failed steps, skipped rooms and policy notes verbatim', async () => {
  const reply = applied({ complete: false, failed: [{ step: '<room> joins', error: 'Speaker offline' }],
    skipped: ['Missing room left alone'], notes: [{ code: 'VOLUME_CLAMPED', detail: 'Volume capped at 30' }] });
  const f = await applyReply(reply);
  assert.equal(f.ok, false);
  assert.equal(f.writes.length, 1);
  assert.deepEqual(f.control.state.result, reply);
  assert.match(f.control.state.message, /in part/);
  assert.equal(Object.isFrozen(f.control.state.result.failed[0]), true);
});

test('malformed and contradictory apply acknowledgements remain uncertain, never replayed', async () => {
  for (const patch of [{ scene: 'Elsewhere' }, { done: '' }, { steps: [42] }, { failed: null },
    { failed: {} }, { skipped: [null] }, { notes: [{ code: 'X' }] },
    { complete: false }, { complete: true, failed: [{ step: 'Join', error: 'Offline' }] },
    { failed: [{ step: 'Join', error: {} }] }, { changed: false }]) {
    const f = await applyReply(applied(patch));
    assert.equal(f.ok, false, JSON.stringify(patch));
    assert.equal(f.control.state.result, null);
    assert.match(f.control.state.message, /uncertain/i);
    assert.equal(f.control.review('apply', 'Evening'), false);
    assert.equal(f.writes.length, 1);
  }
});

test('closing or disposing during the exact-scene read prevents both apply and deletion', async () => {
  for (const action of ['apply', 'delete']) for (const end of ['close', 'dispose']) {
    const entered = deferred(), wait = deferred();
    let writes = 0;
    const control = Scenes.model({ call: async (name) => {
      if (name === 'scenes') return [row()];
      if (name === 'scene') { entered.resolve(); return wait.promise; }
      writes++;
      return action === 'apply' ? applied() : { deleted: 'Evening', done: 'deleted' };
    } });
    await control.refresh();
    control.review(action, 'Evening');
    const work = control.confirm(control.state.pending);
    await entered.promise;
    control[end]();
    wait.resolve(row());
    assert.equal(await work, false, `${action}/${end}`);
    assert.equal(writes, 0, `${action}/${end}`);
    assert.equal(control.state.writing, false);
  }
});

test('closing after dispatch retains the result and cannot overlap a second write', async () => {
  const entered = deferred(), wait = deferred();
  let writes = 0;
  const control = Scenes.model({ call: async (name) => {
    if (name === 'scenes') return [row()];
    if (name === 'scene') return row();
    writes++;
    entered.resolve();
    return wait.promise;
  } });
  await control.refresh();
  control.review('apply', 'Evening');
  const pending = control.state.pending;
  const work = control.confirm(pending);
  await entered.promise;
  control.close();
  assert.equal(await control.refresh(), false);
  assert.equal(await control.confirm(pending), false);
  wait.resolve(applied());
  assert.equal(await work, true);
  assert.equal(control.state.result.done, applied().done);
  assert.equal(writes, 1);
  await control.refresh();
  assert.equal(control.state.result.done, applied().done, 'refresh does not erase the last outcome');
});

test('blank fade and numeric coercion syntax do not silently authorize an immediate apply', async () => {
  const f = fixture();
  await f.control.refresh();
  for (const value of ['', ' ', '0x10', '1e1', '+2', '00']) {
    f.control.editFade(value);
    assert.equal(f.control.review('apply', 'Evening'), false, value);
  }
  assert.deepEqual(f.writes, []);
});

test('draft edits and cancellation cannot strand an active scene-list read', async () => {
  const wait = deferred();
  const control = Scenes.model({ call: () => wait.promise });
  const work = control.refresh();
  control.editName('New name');
  control.editFade('5');
  control.cancel();
  wait.resolve([row()]);
  assert.equal(await work, true);
  assert.equal(control.state.loading, false);
  assert.equal(control.state.loaded, true);
});

test('map key order alone does not change a reviewed scene definition', () => {
  const a = Scenes.sceneOf(row());
  const b = Scenes.sceneOf(row({ volumes: { 'Kitchen@S2': 24, 'Living Room@S2': 28 },
    mutes: { 'Kitchen@S2': false, 'Living Room@S2': false } }));
  assert.equal(Scenes.signature(a), Scenes.signature(b));
});

const topology = () => [
  { household: 'S1', coordinator_room: 'Den', members: ['Den', 'Hall'] },
  { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Studio'] },
];
const SPOTIFY_ID = '0123456789ABCDEFabcdef';
function editorFixture(initial = []) {
  const f = { rows: structuredClone(initial), zones: topology(), writes: [], reads: [],
    favorites: [{ id: 'FV:2/1', title: 'Dinner', kind: 'container', description: 'Owner mix' }],
    moods: { moods: [{ name: 'focus' }] }, hook: null };
  f.call = async (name, options = {}) => {
    if (f.hook) {
      const result = await f.hook(name, options);
      if (result !== undefined) return result;
    }
    f.reads.push({ name, ...structuredClone(options) });
    if (name === 'scenes') return structuredClone(f.rows);
    if (name === 'zones') return structuredClone(f.zones);
    if (name === 'favorites') return structuredClone(f.favorites);
    if (name === 'moods') return structuredClone(f.moods);
    assert.equal(name, 'sceneSave', 'custom scene creation must never send a speaker command');
    f.writes.push({ name, ...structuredClone(options) });
    const saved = { name: options.params.name, saved: 1_900_000_001, ...structuredClone(options.body) };
    for (const g of saved.groups) if (g.source.kind === 'favorite') {
      const favorite = f.favorites.find((v) => v.id === g.source.name);
      g.source.name = favorite.title;
      g.source.uri = 'x-file:server-learned-favorite';
    }
    f.rows = [...f.rows.filter((r) => r.name.toLowerCase() !== saved.name.toLowerCase()), saved];
    return saved;
  };
  f.control = Scenes.model({ call: f.call });
  return f;
}
async function editor(f = editorFixture()) {
  await f.control.refresh();
  assert.equal(await f.control.beginDefinition(), true);
  f.control.editName('Morning');
  return f;
}
function group(f, coordinator, kind = 'keep', value = '', members = []) {
  const c = f.control;
  c.addGroup();
  const index = c.state.draft.groups.length - 1;
  const set = (field, value) => c.editGroup(c.state.draft.groups[index], field, value);
  set('coordinator', coordinator);
  set('kind', kind);
  set('value', value);
  set('members', members);
  return index;
}
async function saveDefinition(f) {
  assert.equal(await f.control.reviewDefinition(), true, f.control.state.message);
  assert.equal(await f.control.confirm(f.control.state.pending), true, f.control.state.message);
  return f.writes[0].body;
}

test('custom editor is lazy, reads household-qualified choices, and does not preselect groups', async () => {
  const f = editorFixture();
  assert.deepEqual(f.reads, []);
  assert.equal(await f.control.beginDefinition(), false);
  await editor(f);
  assert.deepEqual(f.control.state.draft.groups, []);
  assert.deepEqual(f.control.state.rooms.map((r) => r.target), ['Den@S1', 'Hall@S1', 'Den@S2', 'Studio@S2']);
  assert.deepEqual(f.writes, []);
});

test('volume and mute only scenes preserve zero and false and never become a capture', async () => {
  const f = await editor();
  f.control.editRoom('Den@S1', 'volumes', '0');
  f.control.editRoom('Den@S2', 'volumes', '100');
  f.control.editRoom('Hall@S1', 'mutes', 'unmuted');
  f.control.editRoom('Studio@S2', 'mutes', 'muted');
  const body = await saveDefinition(f);
  assert.deepEqual(body, { groups: [], volumes: { 'Den@S1': 0, 'Den@S2': 100 },
    mutes: { 'Hall@S1': false, 'Studio@S2': true } });
  assert.equal(f.writes.length, 1);
  assert.match(f.control.state.message, /No playback was changed/);
  assert.equal(f.control.state.saved.name, 'Morning');
});

test('one scene can define independent S1 and S2 groups with different sources', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'uri', `spotify:playlist:${SPOTIFY_ID}`, ['Hall@S1']);
  group(f, 'Studio@S2', 'dj', '', ['Den@S2']);
  f.control.editRoom('Studio@S2', 'volumes', '25');
  const body = await saveDefinition(f);
  assert.deepEqual(body.groups, [
    { coordinator: 'Den@S1', members: ['Hall@S1'], source: { kind: 'uri', uri: `spotify:playlist:${SPOTIFY_ID}` }, playing: true },
    { coordinator: 'Studio@S2', members: ['Den@S2'], source: { kind: 'dj' }, playing: true },
  ]);
  assert.equal(f.writes.length, 1);
});

test('favorite scenes resolve exact household favorites and accept server-learned URI metadata', async () => {
  for (const value of ['Dinner', 'FV:2/1']) {
    const f = await editor();
    group(f, 'Hall@S1', 'favorite', value);
    const body = await saveDefinition(f);
    assert.deepEqual(body.groups[0].source, { kind: 'favorite', name: 'FV:2/1' });
    assert.equal(f.control.state.saved.groups[0].source.name, 'Dinner');
    assert.equal(f.reads.filter((r) => r.name === 'favorites').length, 2);
    assert.ok(f.reads.filter((r) => r.name === 'favorites').every((r) => r.query.zone === 'Hall@S1'));
  }
});

test('named DJ moods are resolved through the destination rather than guessed or silently defaulted', async () => {
  const f = await editor();
  group(f, 'Studio@S2', 'dj', 'FOCUS');
  const body = await saveDefinition(f);
  assert.deepEqual(body.groups[0].source, { kind: 'dj', mood: 'focus' });
  assert.ok(f.reads.filter((r) => r.name === 'moods').every((r) => r.query.zone === 'Studio@S2'));
});

test('Spotify share links and legacy URIs normalize to the same saved source without link fetches', () => {
  for (const [raw, kind] of [[`Spotify:Album:${SPOTIFY_ID}`, 'album'],
    [`https://open.spotify.com/intl-fr/embed/track/${SPOTIFY_ID}?si=abc#fragment`, 'track'],
    [`https://play.spotify.com/user/me/playlist/${SPOTIFY_ID}`, 'playlist'],
    [`spotify:user:me:playlist:${SPOTIFY_ID}`, 'playlist']]) {
    assert.equal(Scenes.sourceUri(raw), `spotify:${kind}:${SPOTIFY_ID}`);
  }
  assert.equal(Scenes.sourceUri('x-file-cifs:owner/music.flac'), 'x-file-cifs:owner/music.flac');
  for (const value of ['Spotify:artist:' + SPOTIFY_ID, 'spotify:track:short',
    'https://spotify.link/short', 'not a URI', 'x-file:a b', 'x-file:\u0000', 'x-file:' + 'é'.repeat(1100)]) {
    assert.throws(() => Scenes.sourceUri(value), value);
  }
});

test('empty custom definitions cannot trigger the PUT endpoint implicit capture behavior', async () => {
  const f = await editor();
  assert.equal(await f.control.reviewDefinition(), false);
  assert.match(f.control.state.message, /at least one/);
  assert.deepEqual(f.writes, []);
  assert.equal(f.control.review('capture'), false, 'capture is a different explicit workflow');
});

test('cross-household and overlapping group definitions fail before saving', async () => {
  for (const [leader, members] of [['Den@S1', ['Den@S2']], ['Den@S1', ['Den@S1']]]) {
    const f = await editor();
    group(f, leader, 'keep', '', members);
    assert.equal(await f.control.reviewDefinition(), false);
    assert.deepEqual(f.writes, []);
  }
  const f = await editor();
  group(f, 'Den@S1', 'keep', '', ['Hall@S1']);
  group(f, 'Hall@S1');
  assert.equal(await f.control.reviewDefinition(), false);
  assert.match(f.control.state.message, /only one group/);
});

test('unknown targets, bad volumes and bad mute choices never become inferred settings', async () => {
  const f = await editor();
  assert.equal(f.control.editRoom('Not in the house', 'volumes', '20'), false);
  for (const [field, value] of [['volumes', '101'], ['volumes', '-1'], ['volumes', '0x20'],
    ['volumes', '1.5'], ['mutes', 'false']]) {
    await f.control.beginDefinition();
    f.control.editRoom('Den@S1', field, value);
    assert.equal(await f.control.reviewDefinition(), false, `${field}/${value}`);
  }
  assert.deepEqual(f.writes, []);
});

test('new source modes clear obsolete fields and require intentional playback semantics', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'uri', `spotify:album:${SPOTIFY_ID}`);
  let g = f.control.state.draft.groups[0];
  f.control.editGroup(g, 'kind', 'keep');
  g = f.control.state.draft.groups[0];
  assert.equal(g.value, '');
  assert.equal(g.playing, 'keep');
  f.control.editGroup(g, 'playing', 'pause');
  const body = await saveDefinition(f);
  assert.deepEqual(body.groups[0], { coordinator: 'Den@S1', members: [], source: { kind: 'keep' }, playing: false });
});

test('case variants of a saved scene are reviewed as replacements instead of new names', async () => {
  const f = await editor(editorFixture([row()]));
  f.control.editName('EVENING');
  f.control.editRoom('Den@S1', 'volumes', '20');
  assert.equal(await f.control.reviewDefinition(), true);
  assert.equal(f.control.state.pending.row.name, 'Evening');
  f.rows[0].volumes['Living Room@S2'] = 80;
  assert.equal(await f.control.confirm(f.control.state.pending), false);
  assert.deepEqual(f.writes, []);
  const capture = fixture();
  await capture.control.refresh();
  capture.control.editName('EVENING');
  capture.control.review('capture');
  assert.equal(capture.control.state.pending.row.name, 'Evening');
});

test('favorite changes after review invalidate the prepared source without saving it', async () => {
  for (const patch of [{ title: 'Different favorite' }, { kind: 'track' }, { description: 'Changed contents' }]) {
    const f = await editor();
    group(f, 'Den@S1', 'favorite', 'FV:2/1');
    assert.equal(await f.control.reviewDefinition(), true);
    Object.assign(f.favorites[0], patch);
    assert.equal(await f.control.confirm(f.control.state.pending), false);
    assert.deepEqual(f.writes, []);
  }
});

test('shortcuts, ambiguous favorite titles and unknown moods are not saved as playable sources', async () => {
  for (const source of ['favorite', 'dj']) {
    const f = await editor();
    group(f, 'Den@S1', source, source === 'favorite' ? 'FV:2/1' : 'unknown mood');
    if (source === 'favorite') f.favorites[0].kind = 'unplayable';
    assert.equal(await f.control.reviewDefinition(), false);
    assert.deepEqual(f.writes, []);
  }
  const f = await editor();
  f.favorites.push({ id: 'FV:2/2', title: 'Dinner', kind: 'track' });
  group(f, 'Den@S1', 'favorite', 'FV:2/1');
  assert.equal(await f.control.reviewDefinition(), false);
  assert.match(f.control.state.message, /ambiguous/);
});

test('group changes after review are refused while harmless topology ordering is allowed', async () => {
  const f = await editor();
  group(f, 'Den@S1');
  await f.control.reviewDefinition();
  f.zones[0].members.push('New room');
  assert.equal(await f.control.confirm(f.control.state.pending), false);
  assert.deepEqual(f.writes, []);
  const unchanged = await editor();
  group(unchanged, 'Den@S1');
  await unchanged.control.reviewDefinition();
  unchanged.zones.reverse();
  unchanged.zones[1].members.reverse();
  assert.equal(await unchanged.control.confirm(unchanged.control.state.pending), true);
});

test('editing a draft invalidates the old review and detached group handlers cannot change replacements', async () => {
  const f = await editor();
  group(f, 'Den@S1');
  await f.control.reviewDefinition();
  const old = f.control.state.pending, g = f.control.state.draft.groups[0];
  f.control.editGroup(g, 'playing', 'play');
  assert.equal(await f.control.confirm(old), false);
  assert.equal(f.control.editGroup(g, 'playing', 'pause'), false, 'obsolete group object');
  f.control.removeGroup(f.control.state.draft.groups[0]);
  group(f, 'Den@S2');
  assert.equal(f.control.editGroup(g, 'coordinator', 'Hall@S1'), false);
  assert.deepEqual(f.writes, []);
});

test('closing during favorite preflight cancels custom creation before the storage write', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite', 'Dinner');
  await f.control.reviewDefinition();
  const entered = deferred(), wait = deferred();
  f.hook = async (name) => { if (name === 'favorites') { entered.resolve(); return wait.promise; } };
  const work = f.control.confirm(f.control.state.pending);
  await entered.promise;
  f.control.close();
  wait.resolve(f.favorites);
  assert.equal(await work, false);
  assert.deepEqual(f.writes, []);
});

test('lost custom-save responses keep the draft read-only and never resubmit the storage mutation', async () => {
  const f = await editor();
  f.control.editRoom('Den@S1', 'volumes', '10');
  await f.control.reviewDefinition();
  let attempts = 0;
  f.hook = async (name) => { if (name === 'sceneSave') { attempts++; throw new Error('Connection lost'); } };
  const pending = f.control.state.pending;
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(await f.control.reviewDefinition(), false);
  assert.equal(attempts, 1);
  assert.match(f.control.state.message, /uncertain/);
});

test('saved custom-scene acknowledgement must match all authored fields', async () => {
  const f = await editor();
  f.control.editRoom('Den@S1', 'volumes', '10');
  await f.control.reviewDefinition();
  f.hook = async (name) => name === 'sceneSave'
    ? { name: 'Morning', saved: 1900000001, groups: [], volumes: { 'Den@S1': 99 }, mutes: {} } : undefined;
  assert.equal(await f.control.confirm(f.control.state.pending), false);
  assert.match(f.control.state.message, /did not confirm/);
  assert.equal(f.control.state.saved, null);
});

test('scene names count Unicode characters just like the daemon and are bounded to 64', async () => {
  const f = await editor();
  f.control.editRoom('Den@S1', 'mutes', 'muted');
  f.control.editName('🎵'.repeat(65));
  assert.equal(await f.control.reviewDefinition(), false);
  f.control.editName('🎵'.repeat(64));
  assert.equal(await f.control.reviewDefinition(), true);
});

test('malformed topology and late editor reads cannot invent rooms or resurrect a closed draft', async () => {
  for (const zones of [{}, [{ household: 'S1', coordinator_room: 'A', members: [] }],
    [{ household: 'S1', coordinator_room: 'A', members: ['A', 'A'] }]]) {
    assert.throws(() => Scenes.roomsOf(zones));
  }
  const f = editorFixture();
  await f.control.refresh();
  const wait = deferred();
  f.hook = async (name) => name === 'zones' ? wait.promise : undefined;
  const work = f.control.beginDefinition();
  f.control.close();
  wait.resolve(topology());
  assert.equal(await work, false);
  assert.equal(f.control.state.draft, null);
});

test('custom definitions cross a real localhost JSON endpoint with exact encoded names and no playback route', async (t) => {
  const http = require('node:http');
  const { once } = require('node:events');
  const requests = [];
  const favorites = [{ id: 'FV:2/1', title: 'Dinner mix', kind: 'container', description: 'Fixture playlist' }];
  const server = http.createServer(async (req, res) => {
    let input = '';
    for await (const chunk of req) input += chunk;
    requests.push({ method: req.method, path: req.url, body: input ? JSON.parse(input) : null });
    const reply = req.url === '/scenes' ? [] : req.url === '/zones' ? topology()
      : req.url.startsWith('/favorites?') ? favorites
      : { name: decodeURIComponent(req.url.slice('/scenes/'.length)), saved: 1900000001, ...JSON.parse(input) };
    if (req.method === 'PUT') for (const g of reply.groups) if (g.source.kind === 'favorite') {
      g.source.name = favorites.find((f) => f.id === g.source.name).title;
      g.source.uri = 'x-file:fixture-server-metadata';
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(reply));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => new Promise((resolve) => { server.close(resolve); server.closeAllConnections(); }));
  const routes = { scenes: ['GET', '/scenes'], zones: ['GET', '/zones'], favorites: ['GET', '/favorites'], ...Scenes.routes };
  const call = async (name, { params = {}, query = {}, body } = {}) => {
    const [method, template] = routes[name];
    const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]))
      + (Object.keys(query).length ? `?${new URLSearchParams(query)}` : '');
    const response = await fetch(`http://127.0.0.1:${server.address().port}${path}`, {
      method, headers: { 'content-type': 'application/json' },
      ...(method === 'GET' ? {} : { body: JSON.stringify(body) }),
    });
    assert.equal(response.status, 200);
    return response.json();
  };
  const control = Scenes.model({ call });
  await control.refresh();
  await control.beginDefinition();
  const name = 'Dinner / ? # % & 二';
  control.editName(name);
  group({ control }, 'Hall@S1', 'favorite');
  assert.equal(await control.loadSources(control.state.draft.groups[0]), true);
  assert.equal(control.chooseSource(control.state.draft.groups[0], control.choicesFor(control.state.draft.groups[0]).items[0]), true);
  control.editRoom('Den@S1', 'volumes', '0');
  control.editRoom('Den@S2', 'mutes', 'unmuted');
  await control.reviewDefinition();
  const pending = control.state.pending;
  const first = control.confirm(pending);
  assert.equal(await control.confirm(pending), false);
  assert.equal(await first, true, control.state.message);
  assert.deepEqual(requests.filter((r) => r.method !== 'GET'), [{ method: 'PUT',
    path: `/scenes/${encodeURIComponent(name)}`,
    body: { groups: [{ coordinator: 'Hall@S1', members: [], source: { kind: 'favorite', name: 'FV:2/1' }, playing: true }],
      volumes: { 'Den@S1': 0 }, mutes: { 'Den@S2': false } } }]);
  assert.equal(requests.filter((r) => r.path === '/favorites?zone=Hall%40S1').length, 3,
    'browse, review and confirmation all read the exact household-qualified source');
});

test('scene music choices offer playable unique-title favorites and preserve useful descriptions', () => {
  const choices = Scenes.sourceChoicesOf('favorite', [
    { id: 'FV:2/1', title: 'Dinner', kind: 'container', description: 'Owner playlist' },
    { id: 'FV:2/2', title: 'Radio', kind: 'stream' },
    { id: 'FV:2/3', title: 'Recording', kind: 'track' },
    { id: 'FV:2/4', title: 'Shortcut', kind: 'unplayable' },
    { id: 'FV:2/5', title: 'Duplicate', kind: 'track' },
    { id: 'FV:2/6', title: 'DUPLICATE', kind: 'container' },
  ]);
  assert.deepEqual(choices.items.map((f) => f.id), ['FV:2/1', 'FV:2/2', 'FV:2/3']);
  assert.equal(choices.items[0].description, 'Owner playlist');
  assert.equal(choices.ambiguous, 2);
  assert.ok(Object.isFrozen(choices.items[0]));
  const conflict = Scenes.sourceChoicesOf('favorite', [
    { id: 'FV:2/1', title: 'Radio', kind: 'stream' },
    { id: 'FV:2/2', title: 'radio', kind: 'unplayable' },
  ]);
  assert.deepEqual(conflict.items, [], 'a nonplayable duplicate still makes the saved title ambiguous');
});

test('unreadable and ambiguous catalogs never become selectable music', () => {
  for (const value of [null, {}, [{ id: 'FV:2/1', title: 'A' }],
    [{ id: 'FV:2/1', title: 'A', kind: 'track', description: {} }],
    [{ id: 'FV:2/1', title: 'A', kind: 'track' }, { id: 'FV:2/1', title: 'B', kind: 'track' }]]) {
    assert.throws(() => Scenes.sourceChoicesOf('favorite', value));
  }
  for (const value of [null, {}, { moods: [{ name: '' }] }, { moods: [{ name: 'focus' }, { name: 'FOCUS' }] }]) {
    assert.throws(() => Scenes.sourceChoicesOf('dj', value));
  }
  assert.deepEqual(Scenes.sourceChoicesOf('favorite', []).items, []);
  assert.deepEqual(Scenes.sourceChoicesOf('dj', { moods: [] }).items, []);
});

test('favorite browsing and selecting is read-only until the existing custom-scene confirmation', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  let g = f.control.state.draft.groups[0];
  assert.equal(await f.control.loadSources(g), true);
  const choice = f.control.choicesFor(g).items[0];
  assert.equal(f.control.chooseSource(g, choice), true);
  g = f.control.state.draft.groups[0];
  assert.equal(g.value, choice.id);
  assert.equal(g.choice.title, 'Dinner');
  assert.deepEqual(f.writes, []);
  assert.equal(await f.control.reviewDefinition(), true);
  assert.equal(f.control.state.pending.prepared.preview.groups[0].source.name, 'Dinner');
  assert.deepEqual(f.writes, []);
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.equal(f.writes.length, 1);
  assert.deepEqual(f.writes[0].body.groups[0].source, { kind: 'favorite', name: 'FV:2/1' });
});

test('favorite search, paging and no-result filters retain the chosen music without another request', async () => {
  const f = await editor();
  f.favorites = Array.from({ length: 15 }, (_, i) => ({ id: `FV:2/${i + 1}`, title: `Favorite ${i + 1}`,
    kind: 'track', description: i === 12 ? 'Evening cello recording' : 'Piano' }));
  group(f, 'Den@S1', 'favorite');
  let g = f.control.state.draft.groups[0];
  await f.control.loadSources(g);
  f.control.chooseSource(g, f.control.choicesFor(g).items[0]);
  g = f.control.state.draft.groups[0];
  const reads = f.reads.length;
  assert.equal(f.control.pageSources(g, 1), true);
  assert.equal(Scenes.sourcePage(f.control.choicesFor(g)).items[0].title, 'Favorite 7');
  assert.equal(f.control.pageSources(g, 1), true);
  assert.equal(Scenes.sourcePage(f.control.choicesFor(g)).items.length, 3);
  assert.equal(f.control.pageSources(g, 1), false);
  assert.equal(f.control.filterSources(g, 'CELLO'), true);
  assert.deepEqual(Scenes.sourcePage(f.control.choicesFor(g)).items.map((item) => item.title), ['Favorite 13']);
  assert.equal(f.control.filterSources(g, 'No match'), true);
  assert.equal(Scenes.sourcePage(f.control.choicesFor(g)).total, 0);
  assert.equal(g.choice.title, 'Favorite 1');
  assert.equal(f.reads.length, reads);
  assert.deepEqual(f.writes, []);
});

test('each group browses its own household and cannot select another group catalog item', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  group(f, 'Den@S2', 'favorite');
  f.hook = async (route, options) => route === 'favorites'
    ? [{ id: 'FV:2/1', title: `${options.query.zone} collection`, kind: 'container' }] : undefined;
  const [a, b] = f.control.state.draft.groups;
  await Promise.all([f.control.loadSources(a), f.control.loadSources(b)]);
  const choiceA = f.control.choicesFor(a).items[0], choiceB = f.control.choicesFor(b).items[0];
  assert.equal(choiceA.title, 'Den@S1 collection');
  assert.equal(choiceB.title, 'Den@S2 collection');
  assert.equal(f.control.chooseSource(b, choiceA), false);
  assert.equal(f.control.chooseSource(b, choiceB), true);
  assert.equal(f.control.state.draft.groups[1].choice.title, 'Den@S2 collection');
  assert.equal(f.control.state.draft.groups[0].value, '');
});

test('source reads cannot repopulate a changed destination, source kind, removed group or discarded draft', async () => {
  for (const change of ['coordinator', 'kind', 'remove', 'discard', 'close', 'new draft', 'dispose']) {
    const f = await editor();
    group(f, 'Den@S1', 'favorite');
    const g = f.control.state.draft.groups[0], wait = deferred();
    f.hook = async (name) => name === 'favorites' ? wait.promise : undefined;
    const loading = f.control.loadSources(g);
    if (change === 'coordinator') f.control.editGroup(g, 'coordinator', 'Den@S2');
    if (change === 'kind') f.control.editGroup(g, 'kind', 'dj');
    if (change === 'remove') f.control.removeGroup(g);
    if (change === 'discard') f.control.discardDefinition();
    if (change === 'close') f.control.close();
    if (change === 'new draft') await f.control.beginDefinition();
    if (change === 'dispose') f.control.dispose();
    wait.resolve(f.favorites);
    assert.equal(await loading, false, change);
    assert.notEqual(f.control.choicesFor(g)?.phase, 'ready', change);
    assert.deepEqual(f.writes, []);
  }
});

test('an earlier request cannot replace a new catalog even after returning to the same coordinator and kind', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  let g = f.control.state.draft.groups[0];
  const old = deferred();
  f.hook = async (name) => name === 'favorites' ? old.promise : undefined;
  const first = f.control.loadSources(g);
  assert.equal(await f.control.loadSources(g), false, 'same read does not overlap itself');
  f.control.editGroup(g, 'kind', 'keep');
  g = f.control.state.draft.groups[0];
  f.control.editGroup(g, 'kind', 'favorite');
  g = f.control.state.draft.groups[0];
  f.hook = null;
  f.favorites[0].title = 'Current collection';
  assert.equal(await f.control.loadSources(g), true);
  old.resolve([{ id: 'FV:2/1', title: 'Old collection', kind: 'container' }]);
  assert.equal(await first, false);
  assert.equal(f.control.choicesFor(g).items[0].title, 'Current collection');
});

test('editing room settings during a read retains its correct source catalog', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  const g = f.control.state.draft.groups[0], wait = deferred();
  f.hook = async (name) => name === 'favorites' ? wait.promise : undefined;
  const loading = f.control.loadSources(g);
  f.control.editName('Different scene name');
  f.control.editRoom('Hall@S1', 'volumes', '0');
  f.control.editGroup(g, 'members', ['Hall@S1']);
  wait.resolve(f.favorites);
  assert.equal(await loading, true);
  assert.equal(f.control.choicesFor(f.control.state.draft.groups[0]).items[0].title, 'Dinner');
  assert.equal(f.control.state.draft.volumes['Hall@S1'], '0');
});

test('retrying a failed catalog read keeps selected music and authored settings', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  let g = f.control.state.draft.groups[0];
  await f.control.loadSources(g);
  f.control.chooseSource(g, f.control.choicesFor(g).items[0]);
  g = f.control.state.draft.groups[0];
  f.control.editRoom('Hall@S1', 'mutes', 'unmuted');
  f.hook = async (name) => { if (name === 'favorites') throw new Error('Fixture temporarily unavailable'); };
  assert.equal(await f.control.loadSources(g), false);
  assert.equal(f.control.choicesFor(g).phase, 'error');
  assert.match(f.control.choicesFor(g).error, /temporarily unavailable/);
  assert.equal(g.choice.title, 'Dinner');
  f.hook = null;
  assert.equal(await f.control.loadSources(g), true);
  assert.equal(f.control.choicesFor(g).phase, 'ready');
  assert.equal(f.control.state.draft.mutes['Hall@S1'], 'unmuted');
  assert.deepEqual(f.writes, []);
});

test('a browsed favorite changed before review must be chosen again instead of silently adopting new content', async () => {
  const f = await editor();
  group(f, 'Den@S1', 'favorite');
  let g = f.control.state.draft.groups[0];
  await f.control.loadSources(g);
  const oldChoice = f.control.choicesFor(g).items[0];
  f.control.chooseSource(g, oldChoice);
  f.favorites[0].title = 'Changed collection';
  assert.equal(await f.control.reviewDefinition(), false);
  assert.match(f.control.state.message, /selected favorite changed/);
  assert.deepEqual(f.writes, []);
  g = f.control.state.draft.groups[0];
  await f.control.loadSources(g);
  assert.equal(f.control.chooseSource(g, oldChoice), false, 'detached catalog item cannot be selected after refresh');
  assert.equal(f.control.chooseSource(g, f.control.choicesFor(g).items[0]), true);
  assert.equal(await f.control.reviewDefinition(), true);
  assert.equal(f.control.state.pending.prepared.preview.groups[0].source.name, 'Changed collection');
});

test('named and automatic DJ choices use the same reviewed scene source contract', async () => {
  for (const automatic of [false, true]) {
    const f = await editor();
    group(f, 'Studio@S2', 'dj');
    let g = f.control.state.draft.groups[0];
    await f.control.loadSources(g);
    assert.equal(f.control.chooseSource(g, f.control.choicesFor(g).items[0]), true);
    if (automatic) {
      g = f.control.state.draft.groups[0];
      assert.equal(f.control.chooseSource(g, null), true);
    }
    const body = await saveDefinition(f);
    assert.deepEqual(body.groups[0].source, automatic ? { kind: 'dj' } : { kind: 'dj', mood: 'focus' });
    assert.ok(f.reads.filter((r) => r.name === 'moods').every((r) => r.query.zone === 'Studio@S2'));
  }
});

test('automatic DJ remains available when named moods cannot be read', async () => {
  const f = await editor();
  group(f, 'Studio@S2', 'dj');
  f.hook = async (name) => { if (name === 'moods') throw new Error('Fixture mood service unavailable'); };
  const g = f.control.state.draft.groups[0];
  assert.equal(await f.control.loadSources(g), false);
  assert.equal(f.control.chooseSource(g, null), true);
  const body = await saveDefinition(f);
  assert.deepEqual(body.groups[0].source, { kind: 'dj' });
});
