'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { model, LIMIT, MAX_QUERY } = require('../crates/fsonos-api/src/remote/library.js');
const houses = [
  { household: 'S1', coordinator_room: 'Room', members: ['Room', 'Study'] },
  { household: 'S2', coordinator_room: 'Room', members: ['Room'] },
];
const track = { kind: 'track', title: 'First song', subtitle: 'Test Artist', source_uri: 'spotify:track:Track01' };
const album = { kind: 'album', title: 'Test Album', source_uri: 'spotify:album:Album01' };
const playlist = { kind: 'playlist', title: 'Test Playlist', source_uri: 'spotify:playlist:Playlist01' };
const favorite = { kind: 'favorite', title: 'Station', subtitle: 'Synthetic station', favorite: 'FV:2/4' };
const outcome = { done: 'Playing the selected source', changed: true, notes: [{ detail: 'Volume capped first' }] };
const added = { done: 'Queued without starting playback', first: 2, count: 3, playing_queue: false };
const later = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
function fixture(override = async () => undefined) {
  const calls = [];
  let refreshed = 0;
  const control = model({ call: async (name, args) => {
    calls.push([name, args]);
    const supplied = await override(name, args);
    if (supplied !== undefined) return supplied;
    if (name === 'zones') return structuredClone(houses);
    if (name === 'librarySearch') return structuredClone([track, album, playlist, favorite]);
    if (name === 'favorites') return [{ id: favorite.favorite, title: favorite.title,
      description: favorite.subtitle, kind: 'stream' }];
    if (name === 'libraryAdd') return added;
    return outcome;
  }, played: () => { refreshed++; } });
  return { control, calls, writes: () => calls.filter(([name]) => ['libraryPlay', 'libraryAdd', 'playFavorite'].includes(name)),
    refreshed: () => refreshed };
}
async function ready(f) {
  assert.equal(await f.control.refreshRooms(), true);
  assert.equal(f.control.target('Room@S2'), true);
  f.control.query('  Test Artist  ');
  assert.equal(await f.control.search(), true);
}

test('initialization is lazy and never picks a destination for the user', async () => {
  const f = fixture();
  assert.equal(f.calls.length, 0);
  await f.control.refreshRooms();
  assert.equal(f.control.state.target, '');
  assert.equal(f.control.state.zones.length, 2);
  assert.equal(f.control.state.zones[0].target, 'Room@S1');
  f.control.query('music');
  assert.equal(await f.control.search(), false);
  assert.equal(f.calls.length, 1);
});
test('search binds the query and favorites to the explicitly qualified group', async () => {
  const f = fixture(); await ready(f);
  assert.deepEqual(f.calls[1], ['librarySearch', { query: { q: 'Test Artist', zone: 'Room@S2', limit: LIMIT } }]);
  assert.deepEqual(f.control.state.hits.map((hit) => hit.kind), ['track', 'album', 'playlist', 'favorite']);
  assert.equal(f.writes().length, 0);
});
test('empty and overlong searches send no library request', async () => {
  const f = fixture(); await ready(f);
  const before = f.calls.length;
  for (const value of [' ', 'x'.repeat(MAX_QUERY + 1)]) {
    f.control.query(value);
    assert.equal(await f.control.search(), false);
  }
  assert.equal(f.calls.length, before);
});
test('play freezes source and target and requires separate confirmation', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.control.choose(0, 'play'), true);
  assert.equal(f.writes().length, 0);
  assert.ok(Object.isFrozen(f.control.state.pending.body));
  assert.equal(await f.control.confirm(), true);
  assert.deepEqual(f.writes(), [['libraryPlay', { body: { zone: 'Room@S2', source_uri: track.source_uri, title: track.title } }]]);
  assert.match(f.control.state.message, /Volume capped first/);
  assert.equal(await f.control.confirm(), false);
});
test('album and playlist additions use queue insertion, not source replacement', async () => {
  for (const [index, mode, next] of [[1, 'append', false], [2, 'next', true]]) {
    const f = fixture(); await ready(f);
    f.control.choose(index, mode);
    assert.equal(await f.control.confirm(), true);
    assert.deepEqual(f.writes(), [['libraryAdd', { body: { zone: 'Room@S2', source_uri: [track, album, playlist][index].source_uri,
      title: [track, album, playlist][index].title, next } }]]);
  }
});
test('favorite playback re-reads its household and never substitutes a Spotify source', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.control.choose(3, 'append'), false);
  f.control.choose(3, 'play');
  assert.equal(await f.control.confirm(), true);
  assert.deepEqual(f.calls.find(([name]) => name === 'favorites'), ['favorites', { query: { zone: 'Room@S2' } }]);
  assert.deepEqual(f.writes(), [['playFavorite', { body: { zone: 'Room@S2', favorite: 'FV:2/4' } }]]);
});
test('changed or ambiguous favorite metadata refuses playback before dispatch', async () => {
  for (const records of [[], [{ id: 'FV:2/4', title: 'Different', kind: 'track' }],
    [{ id: favorite.favorite, title: favorite.title, description: favorite.subtitle, kind: 'unplayable' }]]) {
    const f = fixture(async (name) => name === 'favorites' ? records : undefined); await ready(f);
    f.control.choose(3, 'play');
    assert.equal(await f.control.confirm(), false);
    assert.equal(f.writes().length, 0);
    assert.match(f.control.state.message, /No playback request was sent/);
  }
});
test('a changed group invalidates the destination before a music write', async () => {
  let changed = false;
  const f = fixture(async (name) => name === 'zones' && changed
    ? [{ ...houses[1], members: ['Room', 'New room'] }] : undefined);
  await ready(f); f.control.choose(0, 'play'); changed = true;
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.writes().length, 0);
  assert.equal(f.control.state.target, '');
});
test('group member ordering alone does not invalidate the observed destination', async () => {
  let count = 0;
  const f = fixture(async (name) => name === 'zones' && ++count > 1
    ? [{ ...houses[0], members: ['Study', 'Room'] }, houses[1]] : undefined);
  await ready(f); f.control.target('Room@S1'); await f.control.search();
  f.control.choose(0, 'play'); assert.equal(await f.control.confirm(), true);
});
test('target, query, refresh and cancel each invalidate review consent', async () => {
  for (const invalidate of [(c) => c.target('Room@S1'), (c) => c.query('changed'),
    (c) => c.refreshRooms(), (c) => c.cancel()]) {
    const f = fixture(); await ready(f); f.control.choose(1, 'play');
    await invalidate(f.control);
    assert.equal(await f.control.confirm(), false);
    assert.equal(f.writes().length, 0);
  }
});
test('a late search cannot restore old results after the query or target changes', async () => {
  const old = later();
  const f = fixture(async (name, args) => name === 'librarySearch' && args.query.q === 'old' ? old.promise : undefined);
  await ready(f); f.control.query('old'); const pending = f.control.search();
  f.control.query('new'); await f.control.search();
  old.resolve([album]); assert.equal(await pending, false);
  assert.equal(f.control.state.hits[0].title, track.title);
  f.control.target('Room@S1'); assert.equal(f.control.state.hits.length, 0);
});
test('double confirmation dispatches once while its first preflight is waiting', async () => {
  let defer = false; const gate = later();
  const f = fixture(async (name) => name === 'zones' && defer ? gate.promise : undefined);
  await ready(f); f.control.choose(0, 'play'); defer = true;
  const pending = f.control.confirm();
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.control.choose(1, 'play'), false);
  assert.equal(f.control.target('Room@S1'), false);
  assert.equal(f.control.query('different'), false);
  gate.resolve(houses); assert.equal(await pending, true);
  assert.equal(f.writes().length, 1);
});
test('closing or disposing during preflight cancels unsent playback', async () => {
  for (const end of ['close', 'dispose']) {
    let defer = false; const gate = later();
    const f = fixture(async (name) => name === 'zones' && defer ? gate.promise : undefined);
    await ready(f); f.control.choose(0, 'play'); defer = true;
    const pending = f.control.confirm(); f.control[end](); gate.resolve(houses);
    assert.equal(await pending, false); assert.equal(f.writes().length, 0);
    assert.equal(f.control.state.writing, false);
  }
});
test('failed or malformed mutation replies never retry and require a new search', async () => {
  for (const reply of [null, {}, { done: 'ok' }, { done: 'ok', changed: true, notes: 'bad' }, 'throw']) {
    const f = fixture(async (name) => {
      if (name !== 'libraryPlay') return undefined;
      if (reply === 'throw') throw new Error('Reply lost');
      return reply;
    });
    await ready(f); f.control.choose(0, 'play');
    assert.equal(await f.control.confirm(), false);
    assert.equal(f.writes().length, 1);
    assert.equal(f.control.choose(0, 'play'), false);
    assert.match(f.control.state.message, /was not repeated/);
  }
});
test('queue-add confirmations reject empty or impossible counters', async () => {
  for (const reply of [{ ...added, count: 0 }, { ...added, first: -1 }, { ...added, count: 1.5 },
    { ...added, playing_queue: 'yes' }]) {
    const f = fixture(async (name) => name === 'libraryAdd' ? reply : undefined);
    await ready(f); f.control.choose(1, 'append');
    assert.equal(await f.control.confirm(), false); assert.equal(f.writes().length, 1);
  }
});
test('unreadable preflight fails without making any music write', async () => {
  let fail = false;
  const f = fixture(async (name) => { if (name === 'zones' && fail) throw new Error('Unreachable'); });
  await ready(f); f.control.choose(0, 'play'); fail = true;
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.writes().length, 0);
  assert.match(f.control.state.message, /No playback request was sent/);
});
test('malformed result lists fail closed and unsupported sources stay read-only', async () => {
  for (const bad of [{}, null, [null], [{ title: 5, kind: 'track' }], Array(LIMIT + 1).fill(track)]) {
    const f = fixture(async (name) => name === 'librarySearch' ? bad : undefined);
    await f.control.refreshRooms(); f.control.target('Room@S2'); f.control.query('music');
    assert.equal(await f.control.search(), false); assert.equal(f.control.choose(0, 'play'), false);
  }
  for (const hit of [{ ...track, source_uri: 'javascript:alert(1)' }, { ...track, kind: 'album' },
    { ...favorite, favorite: 'different' }, { ...track, kind: 'future-kind' }]) {
    const f = fixture(async (name) => name === 'librarySearch' ? [hit] : undefined);
    await ready(f); assert.equal(f.control.choose(0, 'play'), false);
  }
});
test('malformed or duplicate destination groups cannot become writable targets', async () => {
  for (const zones of [null, [{ ...houses[1], members: [] }], [houses[1], houses[1]]]) {
    const f = fixture(async (name) => name === 'zones' ? zones : undefined);
    assert.equal(await f.control.refreshRooms(), false); assert.equal(f.control.state.target, '');
  }
});
test('server errors retain their actionable hints without changing request routing', async () => {
  const f = fixture(async (name) => { if (name === 'librarySearch') throw Object.assign(new Error('Library unavailable'), { hint: 'Sync on daemon host.' }); });
  await f.control.refreshRooms(); f.control.target('Room@S2'); f.control.query('music');
  assert.equal(await f.control.search(), false);
  assert.match(f.control.state.message, /Sync on daemon host/);
  assert.equal(f.writes().length, 0);
});
test('action refresh failures do not turn acknowledged playback into an automatic retry', async () => {
  const calls = [];
  const c = model({ call: async (name) => { calls.push(name); return name === 'zones' ? houses : name === 'librarySearch' ? [track] : outcome; },
    played: () => { throw new Error('read refresh failed'); } });
  await c.refreshRooms(); c.target('Room@S2'); c.query('music'); await c.search(); c.choose(0, 'play');
  assert.equal(await c.confirm(), true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(calls.filter((name) => name === 'libraryPlay').length, 1);
});

const recentRecords = [
  { source_uri: track.source_uri, title: track.title, artist: track.subtitle, played_at: 1700000030 },
  { source_uri: track.source_uri, title: track.title, artist: track.subtitle, played_at: 1700000000 },
  { source_uri: album.source_uri, title: album.title, played_at: 1699990000 },
];
test('recent listening is group-scoped, lazy and independent of search words', async () => {
  const f = fixture(async (name) => name === 'libraryHistory' ? recentRecords : undefined);
  await f.control.refreshRooms(); f.control.target('Room@S1');
  assert.equal(f.calls.length, 1);
  assert.equal(await f.control.recent(), true);
  assert.deepEqual(f.calls[1], ['libraryHistory', { query: { zone: 'Room@S1', limit: LIMIT } }]);
  assert.equal(f.control.state.view, 'history');
  assert.deepEqual(f.control.state.hits.map((hit) => hit.playedAt), recentRecords.map((r) => r.played_at));
  assert.equal(f.control.state.hits.length, 3, 'repeated listens are not collapsed');
});
test('recent music replays the recorded URI or appends it without any fuzzy search', async () => {
  for (const mode of ['play', 'append', 'next']) {
    const f = fixture(async (name) => name === 'libraryHistory' ? recentRecords : undefined);
    await f.control.refreshRooms(); f.control.target('Room@S2'); await f.control.recent();
    f.control.choose(2, mode);
    assert.equal(await f.control.confirm(), true);
    assert.equal(f.calls.some(([name]) => name === 'librarySearch'), false);
    assert.equal(f.writes()[0][1].body.source_uri, album.source_uri);
    assert.equal(f.writes()[0][0], mode === 'play' ? 'libraryPlay' : 'libraryAdd');
  }
});
test('recent history without cached names still has a playable Spotify identity', async () => {
  const f = fixture(async (name) => name === 'libraryHistory'
    ? [{ source_uri: playlist.source_uri, played_at: 1700000000 }] : undefined);
  await f.control.refreshRooms(); f.control.target('Room@S2'); await f.control.recent();
  assert.equal(f.control.state.hits[0].title, playlist.source_uri);
  assert.equal(f.control.choose(0, 'next'), true);
  assert.equal(await f.control.confirm(), true);
});
test('unsupported historical streams remain visible but are not replayed as guesses', async () => {
  const f = fixture(async (name) => name === 'libraryHistory'
    ? [{ source_uri: 'x-rincon-mp3radio://example.invalid/expired', title: 'Old temporary stream', played_at: 1700000000 }] : undefined);
  await f.control.refreshRooms(); f.control.target('Room@S2'); await f.control.recent();
  assert.equal(f.control.state.hits[0].title, 'Old temporary stream');
  for (const mode of ['play', 'append', 'next']) assert.equal(f.control.choose(0, mode), false);
  assert.equal(f.writes().length, 0);
});
test('history read failure is distinct from an empty successful history', async () => {
  for (const value of [[], null, {}, [{ source_uri: 'spotify:track:Test', played_at: 'yesterday' }],
    [{ source_uri: 'spotify:track:Test', played_at: 8640000000001 }], Array(LIMIT + 1).fill(recentRecords[0])]) {
    const f = fixture(async (name) => name === 'libraryHistory' ? value : undefined);
    await f.control.refreshRooms(); f.control.target('Room@S2');
    assert.equal(await f.control.recent(), Array.isArray(value) && value.length === 0);
    assert.equal(f.control.state.loaded, Array.isArray(value) && value.length === 0);
    assert.equal(f.writes().length, 0);
  }
});
test('changing between search and history invalidates previous review consent', async () => {
  const f = fixture(async (name) => name === 'libraryHistory' ? recentRecords : undefined); await ready(f);
  f.control.choose(0, 'play'); await f.control.recent();
  assert.equal(await f.control.confirm(), false);
  f.control.choose(0, 'play'); await f.control.search();
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.writes().length, 0);
});
test('a late history reply cannot replace newer search results', async () => {
  const gate = later();
  const f = fixture(async (name) => name === 'libraryHistory' ? gate.promise : undefined); await ready(f);
  const pending = f.control.recent();
  f.control.query('another song'); await f.control.search(); gate.resolve(recentRecords);
  assert.equal(await pending, false);
  assert.equal(f.control.state.view, 'search');
  assert.equal(f.control.state.hits[0].playedAt, undefined);
});
test('stale rendered result handlers cannot select a newer item in the same slot', async () => {
  const f = fixture(async (name) => name === 'libraryHistory' ? recentRecords : undefined); await ready(f);
  const stale = f.control.state.hits[0];
  await f.control.recent();
  assert.equal(f.control.choose(0, 'play', stale), false);
  assert.equal(f.control.choose(0, 'play', f.control.state.hits[0]), true);
});
test('history cannot be read during a write or without a destination', async () => {
  const gate = later();
  const f = fixture(async (name) => name === 'libraryPlay' ? gate.promise : undefined);
  await f.control.refreshRooms(); assert.equal(await f.control.recent(), false);
  f.control.target('Room@S2'); f.control.query('artist'); await f.control.search();
  f.control.choose(0, 'play'); const writing = f.control.confirm();
  assert.equal(await f.control.recent(), false); gate.resolve(outcome); await writing;
  assert.equal(f.calls.some(([name]) => name === 'libraryHistory'), false);
});
