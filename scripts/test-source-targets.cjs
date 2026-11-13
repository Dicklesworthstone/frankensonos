// Reviewed source sets through the shipped phone controller and shared helpers.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const T = require('../crates/fsonos-api/src/remote/source-targets.js');
const R = require('../crates/fsonos-api/src/remote/room-sets.js');
const ID = '0123456789ABCDEFGHIJKL';
const target = 'Evening & upstairs/#';
const room = (household, name, player, coordinator = player) => ({ household, room: name, player, coordinator });
const defer = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };
function fixture() {
  const rooms = [room('S1', 'Den', 'S1_DEN'), room('S1', 'Office', 'S1_OFFICE', 'S1_DEN'),
    room('S2', 'Den', 'S2_DEN'), room('S2', 'Kitchen', 'S2_KITCHEN')];
  const resolution = { target, matched: 'alias', rooms, groups: [
    { household: 'S1', coordinator: 'S1_DEN', coordinator_room: 'Den', members: rooms.slice(0, 2) },
    { household: 'S2', coordinator: 'S2_DEN', coordinator_room: 'Den', members: [rooms[2]] },
    { household: 'S2', coordinator: 'S2_KITCHEN', coordinator_room: 'Kitchen', members: [rooms[3]] },
  ] };
  const zones = resolution.groups.map(g => ({ household: g.household, coordinator_room: g.coordinator_room,
    members: g.members.map(r => r.room) }));
  const states = new Map(zones.map((zone, i) => [`${zone.coordinator_room}@${zone.household}`,
    { zone, transport_state: i === 1 ? 'playing' : 'paused', track: { uri: i === 1 ? 'sample:radio' : 'sample:queue', title: 'Existing music',
      queue_position: i === 0 ? 2 : null, position_secs: 17 } }]));
  const queues = new Map(zones.map((zone, i) => [`${zone.coordinator_room}@${zone.household}`,
    { zone: zone.coordinator_room, revision: 'queue-' + i, total: i === 2 ? 0 : 4 - i,
      playing: i === 0 ? 2 : null, items: i === 2 ? [] : [{ position: 1 }] }]));
  const playlists = new Map([['S1', [{ id: 'SQ:7', title: 'Evening & Friends' }]], ['S2', [{ id: 'SQ:49', title: 'Evening & Friends' }]]]);
  const favorites = new Map([['S1', [{ id: 'FV:2/7', title: 'Evening & Friends', kind: 'track' }]],
    ['S2', [{ id: 'FV:2/49', title: 'Evening & Friends', kind: 'container' }]]]);
  const h = { resolution, zones, states, queues, playlists, favorites, calls: [], custom: null, executions: 0, response: null };
  h.receipt = () => ({ done: 'Confirmed additions in 3 groups', complete: true,
    added: h.resolution.groups.map(g => { const q = h.queues.get(`${g.coordinator_room}@${g.household}`);
      return { household: g.household, coordinator: g.coordinator,
        queue: { zone: g.coordinator_room, done: 'Added music', first: q.playing === null ? q.total + 1 : q.playing + 1,
          count: 2, playing_queue: q.playing !== null } }; }), failed: [], logged: true, undoable: true });
  h.call = async (name, args = {}) => {
    h.calls.push([name, args]);
    if (h.custom) { const value = h.custom(name, args); if (value !== undefined) return value; }
    let value;
    if (name === 'zones') value = h.zones;
    else if (name === 'sourceTargetResolve') value = { ...h.resolution, target: args.query.target };
    else if (name === 'sourceTargetRooms') value = rooms.map(r => ({ name: r.room, household: r.household, zone: r.room, aliases: [target] }));
    else if (name === 'state') value = h.states.get(args.params.room);
    else if (name === 'sourceQueueState') value = h.queues.get(args.params.room);
    else if (name === 'sourcePlaylists') value = h.playlists.get(args.query.zone.split('@').at(-1));
    else if (name === 'sourceFavorites') value = h.favorites.get(args.query.zone.split('@').at(-1));
    else if (name === 'sourceQueue') value = h.response || h.receipt();
    else if (['sourcePlay', 'sourcePlaylistPlay', 'sourceFavoritePlay'].includes(name)) value = h.response || { done: 'Playback accepted in all groups', changed: true };
    else throw new Error('Unexpected route ' + name);
    return structuredClone(value);
  };
  h.executeSet = async (groups, work) => { h.executions++; h.held = groups; return work(); };
  h.api = P.model({ call: h.call, executeSet: (...args) => h.executeSet(...args) });
  h.writes = () => h.calls.filter(([name]) => ['sourceQueue', 'sourcePlay', 'sourcePlaylistPlay', 'sourceFavoritePlay'].includes(name));
  h.open = async (kind = 'album', mode = 'next') => {
    await h.api.refresh(); h.api.edit('targetMode', 'set'); h.api.edit('targetQuery', target);
    h.api.edit('input', kind === 'stream' ? 'https://audio.example/live?Signature=a%2Bb' : `spotify:${kind}:${ID}`);
    h.api.edit('mode', mode);
  };
  h.review = async (kind, mode) => { await h.open(kind, mode); assert.equal(await h.api.review(), true, h.api.state.message); return h.api.state.pending; };
  return h;
}

for (const mode of ['play', 'next', 'end']) for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) {
  test(`${kind} ${mode} resolves a multi-household alias and sends one source request`, async () => {
    const h = fixture(), p = await h.review(kind, mode);
    assert.equal(p.targets.groups.length, 3); assert.equal(p.targets.resolution.rooms.length, 4);
    assert.ok(Object.isFrozen(p.targets.entries)); assert.equal(h.writes().length, 0);
    assert.equal(await h.api.confirm(p), true); assert.equal(h.executions, 1); assert.equal(h.held.length, 3);
    assert.equal(h.writes().length, 1); assert.equal(await h.api.confirm(p), false);
    assert.deepEqual(h.writes()[0], [mode === 'play' ? 'sourcePlay' : 'sourceQueue', { body: {
      zone: target, source_uri: `spotify:${kind}:${ID}`, ...(mode === 'play' ? {} : { next: mode === 'next' }),
    } }]);
    assert.equal(h.api.state.result.complete, true);
    if (mode !== 'play') {
      for (const name of ['Den@S1', 'Den@S2', 'Kitchen@S2']) assert.ok(h.api.state.result.detail.includes(name));
      assert.match(h.api.state.result.detail, /position 3/); assert.match(h.api.state.result.detail, /position 4/);
      assert.match(h.api.state.result.detail, /position 1/); assert.match(h.api.state.result.detail, /did not start/);
    }
  });
}

test('HTTP stream fanout keeps its exact address and rejects queue modes before reads', async () => {
  const h = fixture(), p = await h.review('stream', 'play');
  assert.equal(await h.api.confirm(p), true);
  assert.equal(h.writes()[0][1].body.source_uri, 'https://audio.example/live?Signature=a%2Bb');
  await h.open('stream', 'next'); const n = h.calls.length;
  assert.equal(await h.api.review(), false); assert.equal(h.calls.length, n); assert.equal(h.writes().length, 1);
});

for (const kind of ['playlist', 'favorite']) for (const mode of ['play', 'next', 'end']) {
  test(`${kind} ${mode} reviews each household's own exact-title match`, async () => {
    const h = fixture(); await h.open('track', mode);
    h.api.edit('sourceType', kind); h.api.edit('nativeQuery', 'evening friends');
    assert.equal(await h.api.review(), true, h.api.state.message);
    const p = h.api.state.pending, ids = p.targets.entries.map(e => e.item.id);
    assert.deepEqual(ids, kind === 'playlist' ? ['SQ:7', 'SQ:49', 'SQ:49'] : ['FV:2/7', 'FV:2/49', 'FV:2/49']);
    const route = kind === 'playlist' ? 'sourcePlaylists' : 'sourceFavorites';
    assert.equal(h.calls.filter(([name]) => name === route).length, 2);
    assert.equal(await h.api.confirm(p), true);
    assert.equal(h.calls.filter(([name]) => name === route).length, 4);
    assert.deepEqual(h.writes()[0], [mode !== 'play' ? 'sourceQueue' : kind === 'playlist' ? 'sourcePlaylistPlay' : 'sourceFavoritePlay',
      { body: { zone: target, [kind]: 'evening friends', ...(mode === 'play' ? {} : { next: mode === 'next' }) } }]);
  });
}

test('native exact-title selection rejects missing, ambiguous, shortcut and unqueueable later-household matches', async () => {
  for (const later of [[], [{ id: 'FV:2/49', title: 'A different favorite', kind: 'track' }],
    [{ id: 'FV:2/49', title: 'Evening & Friends', kind: 'unplayable' }],
    [{ id: 'FV:2/49', title: 'Evening & Friends', kind: 'stream' }],
    [{ id: 'FV:2/49', title: 'Evening & Friends', kind: 'track' }, { id: 'FV:2/50', title: 'Évening Friends', kind: 'track' }]]) {
    const h = fixture(); h.favorites.set('S2', later); await h.open();
    h.api.edit('sourceType', 'favorite'); h.api.edit('nativeQuery', 'Evening & Friends');
    assert.equal(await h.api.review(), false); assert.equal(h.writes().length, 0); assert.match(h.api.state.message, /S2:/);
  }
  for (const query of ['SQ:7', 'FV:2/7', '02', '+2', '---', ' ']) assert.throws(() => T.prepareNative('playlist', query, 'play'));
  assert.equal(T.nameKey('Dvořák: Prélude in D♭ & F♯'), 'dvorak prelude in d flat f sharp');
});

test('a selected-group favorite uses the same resolver but accepts the unchanged single queue receipt', async () => {
  const h = fixture(); h.resolution.groups = h.resolution.groups.slice(0, 1); h.resolution.rooms = h.resolution.rooms.slice(0, 2);
  await h.api.refresh(); h.api.edit('target', 'Den@S1'); h.api.edit('sourceType', 'favorite');
  h.api.edit('nativeQuery', 'Evening & Friends'); h.api.edit('mode', 'next');
  assert.equal(await h.api.review(), true);
  h.response = { zone: 'Den', done: 'Added favorite', first: 3, count: 1, playing_queue: true };
  assert.equal(await h.api.confirm(h.api.state.pending), true);
  assert.equal(h.writes()[0][1].body.zone, 'Den@S1'); assert.match(h.api.state.result.detail, /position 3/);
});

test('UUID or full-audience changes consume set review even when displayed names remain identical', async () => {
  for (const phase of ['review', 'confirm']) {
    const h = fixture(); let reads = 0;
    const change = () => { h.resolution.groups[0].coordinator = 'REPLACEMENT';
      for (const r of h.resolution.groups[0].members) r.coordinator = 'REPLACEMENT';
      h.resolution.groups[0].members[0].player = 'REPLACEMENT'; };
    if (phase === 'review') {
      h.custom = name => { if (name === 'sourceTargetResolve' && ++reads === 2) change(); };
      await h.open(); assert.equal(await h.api.review(), false);
    } else {
      const p = await h.review(); change(); assert.equal(await h.api.confirm(p), false);
    }
    assert.equal(h.writes().length, 0);
  }
});

test('each queue is bound independently; append tolerates progress while next binds exact occurrence', async () => {
  for (const mode of ['next', 'end']) {
    const h = fixture(), p = await h.review('album', mode);
    h.states.get('Den@S1').track.uri = 'sample:next'; h.states.get('Den@S1').track.queue_position++;
    h.queues.get('Den@S1').playing++;
    assert.equal(await h.api.confirm(p), mode === 'end'); assert.equal(h.writes().length, mode === 'end' ? 1 : 0);
  }
  const h = fixture(), p = await h.review(); h.queues.get('Kitchen@S2').revision += '-edited';
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
});

test('changed household native choices prevent the entire mutation at confirm', async () => {
  const h = fixture(); await h.open(); h.api.edit('sourceType', 'playlist'); h.api.edit('nativeQuery', 'Evening & Friends');
  assert.equal(await h.api.review(), true); const p = h.api.state.pending;
  h.playlists.get('S2')[0].id = 'SQ:88';
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
});

test('partial and all-failed queue receipts retain every authoritative error and log/undo status', async () => {
  for (const allFailed of [false, true]) for (const uncertain of [false, true]) {
    const h = fixture(), p = await h.review(), value = h.receipt();
    const rows = value.added.splice(allFailed ? 0 : 1);
    value.failed = rows.map(row => ({ household: row.household, coordinator: row.coordinator, zone: row.queue.zone,
      error: { code: uncertain ? 'QUEUE_EDIT_UNCERTAIN' : 'UPNP_FAULT', detail: uncertain ? 'Speaker reply was lost' : 'Speaker refused this add',
        hint: 'Read this group’s queue before another add. Never repeat the whole batch.', retryable: false } }));
    value.complete = false; value.undoable = !uncertain && !allFailed; h.response = value;
    assert.equal(await h.api.confirm(p), true); assert.equal(h.writes().length, 1);
    assert.equal(h.api.state.result.status, 'accepted'); assert.equal(h.api.state.result.partial, true); assert.equal(h.api.state.result.complete, false);
    assert.match(h.api.state.result.detail, uncertain ? /reply was lost/ : /refused this add/);
    assert.match(h.api.state.result.detail, /Den@S2/); assert.match(h.api.state.result.detail, /Kitchen@S2/);
    if (!value.undoable) assert.match(h.api.state.result.detail, /No safe batch undo/);
  }
});

test('queue receipts cannot hide a missing group, repeat a group, invent success or misidentify a household', async () => {
  for (const corrupt of [v => v.added.pop(), v => { v.added[1] = v.added[0]; },
    v => { v.added[0].household = 'S2'; }, v => { v.added[0].queue.zone = 'Other'; },
    v => { v.added[0].queue.first = 0; }, v => { v.complete = false; }, v => { v.logged = false; }]) {
    const h = fixture(), p = await h.review(), v = h.receipt(); corrupt(v); h.response = v;
    assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain'); assert.equal(h.writes().length, 1);
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 1);
  }
});

test('a valid partial playback outcome preserves skipped groups and cap notes', async () => {
  const h = fixture(), p = await h.review('track', 'play');
  h.response = { done: 'Playback accepted', changed: true, notes: [{ code: 'SKIPPED', detail: 'Den@S2 could not play' },
    { code: 'VOLUME_CAPPED', detail: 'Den@S1 was capped to 30' }] };
  assert.equal(await h.api.confirm(p), true); assert.equal(h.api.state.result.partial, true);
  assert.match(h.api.state.result.detail, /Den@S2/); assert.match(h.api.state.result.detail, /capped to 30/);
});

test('closing or editing during set preflight prevents sending and invalidates stale handlers', async () => {
  for (const close of [false, true]) {
    const h = fixture(), p = await h.review(), gate = defer();
    h.custom = name => name === 'sourceTargetResolve' ? gate.promise : undefined;
    const sending = h.api.confirm(p); await settle();
    if (close) h.api.close(); else h.api.dispose();
    gate.resolve(structuredClone(h.resolution)); assert.equal(await sending, false); assert.equal(h.writes().length, 0);
  }
  const h = fixture(), gate = defer(); await h.open();
  h.custom = name => name === 'sourceTargetResolve' ? gate.promise : undefined;
  const reading = h.api.review(); await settle(); h.api.edit('targetQuery', 'another-alias');
  gate.resolve(structuredClone(h.resolution)); assert.equal(await reading, false); assert.equal(h.api.state.pending, null);
});

test('one sent set action stays single-flight through close/reopen and preserves its late receipt', async () => {
  const h = fixture(), p = await h.review(), gate = defer(); h.custom = name => name === 'sourceQueue' ? gate.promise : undefined;
  const sending = h.api.confirm(p); await settle(); assert.equal(h.writes().length, 1);
  h.api.close(); assert.equal(await h.api.refresh(), false); assert.equal(await h.api.confirm(p), false);
  gate.resolve(h.receipt()); assert.equal(await sending, true); assert.equal(h.writes().length, 1);
  assert.equal(h.api.state.result.complete, true); assert.equal(h.api.state.writing, false);
});

test('lost acknowledgements are never retried and duplicated lane callbacks cannot resend', async () => {
  const h = fixture(), p = await h.review(); h.custom = name => { if (name === 'sourceQueue') throw new Error('Reply lost'); };
  assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain'); assert.equal(h.writes().length, 1);
  const other = fixture(), q = await other.review(); other.executeSet = async (_, work) => { await work(); await work(); };
  assert.equal(await other.api.confirm(q), true); assert.equal(other.writes().length, 1);
});

test('whole-set action lanes release earlier groups when a later destination is busy', async () => {
  const h = fixture(), p = await h.review(), cards = h.zones.map(zone => ({ zone })), held = [], ended = [];
  h.executeSet = R.coordinated({ cards: () => cards, refresh: async () => {},
    shape: zone => P.groups([zone])[0].signature,
    begin: card => { if (held.length === 1) return false; held.push(card); return true; }, end: card => ended.push(card) });
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0); assert.deepEqual(ended, held);
});

test('room/alias suggestions do not imply membership and preserve punctuation-rich targets', async () => {
  const h = fixture(); await h.open(); assert.equal(await h.api.loadTargetSuggestions(), true);
  const options = h.api.state.targetSuggestions.map(v => v.query);
  for (const choice of ['everywhere', 'here', 'all@S1', 'all@S2', 'Den@S1', target]) assert.ok(options.includes(choice));
  assert.equal(h.calls.filter(([name]) => name === 'sourceTargetResolve').length, 0);
  assert.equal(await h.api.review(), true); assert.equal(h.api.state.pending.targets.target, target);
});

test('a source handoff cannot overwrite an unfinished household-title draft', async () => {
  const h = fixture(); await h.api.refresh(); h.api.edit('targetMode', 'set'); h.api.edit('targetQuery', target);
  h.api.edit('sourceType', 'favorite'); h.api.edit('nativeQuery', 'Evening & Friends');
  assert.equal(await h.api.offer({ source_uri: `spotify:track:${ID}` }), false);
  assert.equal(h.api.state.nativeQuery, 'Evening & Friends'); assert.equal(h.api.state.targetMode, 'set');
  assert.equal(h.writes().length, 0);
});
