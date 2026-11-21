'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Q = require('../crates/fsonos-api/src/remote/queue-tracks.js');
const R = require('../crates/fsonos-api/src/remote/library.js');
const id = n => String(n).padStart(22, '0');
const uri = n => `spotify:track:${id(n)}`;
const album = `spotify:album:${id(900)}`, playlist = `spotify:playlist:${id(901)}`;
const copy = value => structuredClone(value);
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
const observe = (offset = 0, changes = {}) => ({ query: { kind: 'tracks', offset, limit: 50 }, revision: 'library-a', ...changes });
const track = n => ({ source_uri: uri(n), title: `Track ${n}` });

function catalog(query) {
  const kind = query.kind || 'tracks', offset = Number(query.offset || 0), total = 112;
  const items = Array.from({ length: Math.min(50, total - offset) }, (_, index) => {
    const position = offset + index + 1;
    return { kind: 'track', source_uri: uri(position), title: `Track ${position}`, subtitle: 'Synthetic artist', explicit: false,
      ...(query.album_uri ? { album_uri: query.album_uri, disc_number: Math.floor((position - 1) / 50) + 1, track_number: (position - 1) % 50 + 1 } : {}),
      ...(query.playlist_uri ? { position: position * 2 - 1 } : {}) };
  });
  const next_offset = offset + items.length < total ? offset + items.length : null;
  if (query.playlist_uri) return { playlist_uri: query.playlist_uri, name: 'Synthetic playlist', q: query.q || '',
    offset, limit: 50, total, cached_total: total, examined: total * 2, source_total: total * 2,
    coverage: 'complete', fetched_at: 100, revision: 'library-a', next_offset, items };
  return { kind, offset, limit: 50, total, album_uri: query.album_uri || null, coverage: 'cached', revision: 'library-a', next_offset, items,
    ...(query.album_uri ? { album_observation: { coverage: 'complete', fetched_at: 100, cached_total: total } } : {}) };
}
async function fixture(collection = 'tracks', rooms = true) {
  const calls = [], zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Room', members: ['Room', 'Study'] }));
  let hook;
  const state = { source: 'fixture:current', queueRevision: 'queue-a' };
  const backend = async (name, args = {}) => {
    calls.push({ name, args: copy(args) });
    const changed = hook?.(name, args); if (changed !== undefined) return changed;
    if (name === 'zones') return rooms ? copy(zones) : [];
    if (name === 'libraryBrowse' || name === 'libraryPlaylist') return catalog(args.query);
    if (name === 'queue') return { zone: 'Room', total: 2, revision: state.queueRevision, playing: 1,
      items: [{ position: 1, title: 'Current queue' }] };
    if (name === 'state') return { zone: copy(zones.find(zone => `${zone.coordinator_room}@${zone.household}` === args.params.room)),
      transport_state: 'paused', track: { uri: state.source, queue_position: 1 } };
    if (name === 'libraryPlayTracks') return { done: 'Playing selected tracks', changed: true };
    if (name === 'libraryTracks') return { done: 'Added selected tracks', zone: 'Room', first: args.body.next ? 2 : 3,
      count: args.body.tracks.length, playing_queue: true };
    throw new Error(`Unexpected ${name}`);
  };
  const control = R.model({ call: backend }); await control.refreshRooms();
  if (rooms) control.target('Room@S2');
  assert.equal(await control.browse('tracks', 0, collection === 'album' ? { uri: album, title: 'Synthetic album' } : null,
    null, collection === 'playlist' ? { uri: playlist, title: 'Synthetic playlist' } : null), true);
  return { control, calls, state, hook(fn) { hook = fn; },
    reads: () => calls.filter(call => call.name === 'libraryBrowse' || call.name === 'libraryPlaylist'),
    writes: () => calls.filter(call => call.args.body !== undefined) };
}
async function selectThreePages(f) {
  assert.equal(f.control.selectTrack(1, true), true);
  assert.equal(await f.control.next(), true);
  assert.equal(f.control.selectTrack(2, true), true);
  assert.equal(await f.control.next(), true);
  assert.equal(f.control.selectTrack(1, true), true);
  assert.deepEqual(f.control.state.basket.map(row => row.item.source), [uri(2), uri(53), uri(102)]);
}

test('basket retains immutable page proof and initial album/listing order across reverse navigation', () => {
  const basket = Q.basket(), item = track(53), page = observe(50);
  basket.add(page, 2, item); basket.add(observe(), 4, track(5)); basket.add(observe(100), 1, track(102));
  assert.deepEqual(basket.rows.map(row => row.item.source_uri), [uri(5), uri(53), uri(102)]);
  item.title = 'Replaced'; page.query.offset = 1000;
  assert.equal(basket.rows[1].item.title, 'Track 53'); assert.equal(basket.rows[1].page.query.offset, 50);
  for (const value of [basket.rows, basket.rows[0], basket.rows[0].item, basket.rows[0].page, basket.rows[0].page.query]) assert.ok(Object.isFrozen(value));
  assert.deepEqual(basket.indices(observe(50)), [2]);
});

test('custom order and deliberate repeats survive later additions without merging equal URIs', () => {
  const basket = Q.basket(); basket.add(observe(), 0, track(1)); basket.add(observe(), 1, track(1));
  const [first, second] = basket.rows;
  assert.equal(basket.move(second.id, -1), true); assert.equal(basket.repeat(first.id), true);
  basket.add(observe(), 2, track(3));
  assert.deepEqual(basket.rows.map(row => row.index), [1, 0, 0, 2]);
  assert.equal(new Set(basket.rows.map(row => row.id)).size, 4);
  assert.deepEqual(basket.indices(observe()), [0, 1, 2]);
  basket.unselect(observe(), 0);
  assert.deepEqual(basket.rows.map(row => row.index), [1, 2]);
});

test('basket bounds all repeated occurrences and stale controls cannot edit a fresh selection', () => {
  const basket = Q.basket(); basket.add(observe(), 0, track(1)); const old = basket.rows[0].id;
  for (let i = 1; i < 10; i++) basket.repeat(old);
  assert.throws(() => basket.repeat(old), /at most 10/);
  assert.throws(() => basket.add(observe(50), 0, track(51)), /at most 10/);
  basket.clear(); basket.add(observe(), 0, track(1));
  assert.equal(basket.remove(old), false); assert.equal(basket.repeat(old), false); assert.equal(basket.move(old, 1), false);
  assert.equal(basket.rows.length, 1);
});

for (const change of [
  { revision: 'other' }, { query: { kind: 'tracks', offset: 50, limit: 50, q: 'other' } },
  { query: { kind: 'tracks', offset: 50, limit: 50, album_uri: album } },
  { query: { kind: 'tracks', offset: 50, limit: 50, playlist_uri: playlist } },
  { albumObservation: { coverage: 'complete', fetched_at: 101, cached_total: 112 } },
]) test(`basket refuses another listing proof: ${JSON.stringify(change)}`, () => {
  const basket = Q.basket(); basket.add(observe(), 0, track(1));
  assert.throws(() => basket.add(observe(50, change), 0, track(51)), /listing changed/);
  assert.equal(basket.rows.length, 1);
});

for (const collection of ['tracks', 'album', 'playlist']) for (const action of ['play', 'next', 'append']) {
  test(`phone ${collection} builds and confirms a three-page ordered ${action} selection once`, async () => {
    const f = await fixture(collection); await selectThreePages(f);
    const [first, middle, last] = f.control.state.basket;
    assert.equal(f.control.moveTrack(last.id, -1), true);
    assert.equal(f.control.moveTrack(last.id, -1), true);
    assert.equal(f.control.repeatTrack(first.id), true);
    assert.equal(f.control.removeTrack(middle.id), true);
    assert.deepEqual(f.control.state.basket.map(row => row.item.source), [uri(102), uri(2), uri(2)]);
    assert.equal(await f.control.previous(), true); assert.deepEqual(f.control.state.selected, []);
    assert.equal(await f.control.previous(), true); assert.deepEqual(f.control.state.selected, [1]);
    const before = f.reads().length;
    assert.equal(await f.control.chooseTracks(action), true); const pending = f.control.state.pending;
    assert.deepEqual(pending.body.tracks.map(row => row.source_uri), [uri(102), uri(2), uri(2)]);
    assert.equal(f.reads().length - before, 2, 'each selected source page is checked once at review');
    assert.equal(f.writes().length, 0);
    assert.equal(await f.control.confirm(pending), true); assert.equal(await f.control.confirm(pending), false);
    assert.equal(f.reads().length - before, 4, 'each source page is also checked once at confirmation');
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].args.body.zone, 'Room@S2');
    assert.equal(f.writes()[0].name, action === 'play' ? 'libraryPlayTracks' : 'libraryTracks');
    assert.equal(f.writes()[0].args.body.expected_revision, 'queue-a');
    assert.deepEqual(f.control.state.basket, []);
    if (collection === 'playlist') {
      assert.ok(f.reads().filter(call => call.args.query.offset > 0).every(call => call.args.query.expected_revision === 'library-a'));
      assert.equal(pending.tracks.rows[0].hit.position, 203, 'original playlist position survives filtered page offsets');
    }
  });
}

for (const boundary of ['review', 'confirmation']) for (const change of ['revision', 'occurrence', 'album observation']) {
  test(`phone checks off-page ${change} at ${boundary} before any batch write`, async () => {
    const f = await fixture('album'); await selectThreePages(f);
    if (boundary === 'confirmation') assert.equal(await f.control.chooseTracks('next'), true);
    f.hook((name, args) => {
      if (name !== 'libraryBrowse' || args.query.offset !== 0) return;
      const page = catalog(args.query);
      if (change === 'revision') page.revision = 'new-library';
      if (change === 'occurrence') page.items[1].title = 'A different recording';
      if (change === 'album observation') page.album_observation.fetched_at++;
      return page;
    });
    assert.equal(await (boundary === 'review' ? f.control.chooseTracks('next') : f.control.confirm()), false);
    assert.equal(f.control.state.pending, null); assert.equal(f.writes().length, 0);
    assert.match(f.control.state.message, /changed/);
  });
}

test('phone changed collection evidence on a page turn clears the selection visibly', async () => {
  const f = await fixture('album'); f.control.selectTrack(0, true);
  f.hook((name, args) => {
    if (name !== 'libraryBrowse' || args.query.offset === 0) return;
    const page = catalog(args.query); page.album_observation.fetched_at++; return page;
  });
  assert.equal(await f.control.next(), false); assert.deepEqual(f.control.state.basket, []);
  assert.match(f.control.state.selectionNotice, /changed.*cleared/); assert.equal(f.writes().length, 0);
});

for (const reset of ['filter', 'refresh', 'category', 'close']) test(`phone ${reset} clears selection and its retained confirmation`, async () => {
  const f = await fixture(); await selectThreePages(f); await f.control.chooseTracks('append'); const pending = f.control.state.pending;
  if (reset === 'filter') f.control.query('new filter');
  else if (reset === 'refresh') await f.control.refresh();
  else if (reset === 'category') await f.control.browse('albums');
  else f.control.close();
  assert.deepEqual(f.control.state.basket, []); assert.equal(await f.control.confirm(pending), false);
  assert.match(f.control.state.selectionNotice, /cleared/); assert.equal(f.writes().length, 0);
});

test('phone keeps cached selection independent of its destination but consumes the old review', async () => {
  const f = await fixture(); await selectThreePages(f); await f.control.chooseTracks('play'); const pending = f.control.state.pending;
  f.control.target('Room@S1'); assert.equal(f.control.state.basket.length, 3);
  assert.equal(await f.control.confirm(pending), false); assert.equal(f.writes().length, 0);
  assert.equal(await f.control.chooseTracks('play'), true); assert.equal(f.control.state.pending.body.zone, 'Room@S1');
});

test('phone caches selections across pages with all speakers offline', async () => {
  const f = await fixture('tracks', false); await selectThreePages(f);
  assert.equal(await f.control.chooseTracks('play'), false); assert.equal(f.writes().length, 0);
  assert.equal(f.control.state.basket.length, 3);
});

test('phone discards a late source-page validation after closing the selection', async () => {
  const f = await fixture(); await selectThreePages(f); const gate = deferred();
  let page;
  f.hook((name, args) => { if (name === 'libraryBrowse') { page = catalog(args.query); return gate.promise; } });
  const preparing = f.control.chooseTracks('append'); await tick(); f.control.close(); gate.resolve(page);
  assert.equal(await preparing, false); assert.equal(f.control.state.pending, null);
  assert.deepEqual(f.control.state.basket, []); assert.equal(f.writes().length, 0);
});

test('phone rechecks playback after off-page reads and cannot replay a lost multi-page batch', async () => {
  const f = await fixture(); await selectThreePages(f); await f.control.chooseTracks('append');
  const gate = deferred(); let page;
  f.hook((name, args) => {
    if (name === 'libraryBrowse' && args.query.offset === 100) { page = catalog(args.query); return gate.promise; }
  });
  const confirming = f.control.confirm(); await tick(); f.state.source = 'fixture:listener-changed'; gate.resolve(page);
  assert.equal(await confirming, false); assert.equal(f.writes().length, 0); assert.match(f.control.state.message, /Playback changed/);
  f.hook(); await f.control.refresh(); await selectThreePages(f); await f.control.chooseTracks('next'); const pending = f.control.state.pending;
  f.hook(name => name === 'libraryTracks' ? Promise.reject(new Error('Lost reply')) : undefined);
  assert.equal(await f.control.confirm(pending), false); assert.equal(await f.control.confirm(pending), false);
  assert.equal(f.writes().length, 1); assert.match(f.control.state.message, /not repeated/); assert.deepEqual(f.control.state.basket, []);
});

for (const change of ['revision', 'malformed']) test(`phone clears retained playlist selections on a ${change} continuation failure`, async () => {
  const f = await fixture('playlist'); f.control.selectTrack(0, true);
  f.hook((name, args) => {
    if (name !== 'libraryPlaylist' || args.query.offset === 0) return;
    const page = catalog(args.query);
    if (change === 'revision') page.revision = 'new-playlist';
    else page.items.pop();
    return page;
  });
  assert.equal(await f.control.next(), false); assert.deepEqual(f.control.state.basket, []);
  assert.deepEqual(f.control.state.selected, []); assert.equal(f.control.state.pending, null);
  assert.match(f.control.state.selectionNotice, /could not be verified.*cleared/);
  assert.equal(await f.control.chooseTracks('append'), false); assert.equal(f.writes().length, 0);
});
