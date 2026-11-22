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
    if (kind === 'albums' || kind === 'playlists') return {
      kind: kind === 'albums' ? 'album' : 'playlist',
      source_uri: `spotify:${kind === 'albums' ? 'album' : 'playlist'}:${id((kind === 'albums' ? 899 : 900) + position)}`,
      title: kind === 'albums' ? `Album ${position}` : `Playlist ${position}`, subtitle: 'Synthetic artist',
      explicit: false, ...(kind === 'albums' ? { cached_tracks: 112 } : {}) };
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
  const state = { source: 'fixture:current', queueRevision: 'queue-a', targetRevision: 'a'.repeat(64), executions: 0 };
  const resolve = target => {
    const selected = zones.filter(zone => !/^all@/.test(target) || zone.household === target.slice(4));
    const groups = selected.map(zone => {
      const coordinator = `fixture-${zone.household}-Room${state.replaced ? '-new' : ''}`;
      return { household: zone.household, coordinator, coordinator_room: 'Room',
        members: zone.members.map(room => ({ household: zone.household, room,
          player: room === 'Room' ? coordinator : `fixture-${zone.household}-${room}`, coordinator })) };
    });
    return { target, revision: state.targetRevision, matched: target.startsWith('all') || target === 'everywhere' ? 'everywhere' : 'alias',
      groups, rooms: groups.flatMap(group => group.members) };
  };
  const queueFor = target => ({ zone: 'Room', total: target.endsWith('@S1') ? 2 : 5,
    revision: state.queueRevision + '-' + target, playing: target.endsWith('@S1') ? 1 : null,
    items: [{ position: 1, title: 'Current queue' }] });
  const receipt = body => {
    const groups = resolve(body.zone).groups;
    return body.mode === 'play' ? { done: 'Playing selected tracks', changed: true, ...(groups.length > 1 ? { batch: {
      complete: true, results: groups.map(group => ({ household: group.household, target: group.coordinator, zone: group.coordinator_room,
        result: { status: 'succeeded', done: 'Started selected tracks', changed: true } })) } } : {}) }
      : { done: 'Added selected tracks', complete: true, logged: true, undoable: true, failed: [],
        added: groups.map(group => {
          const queue = queueFor(`${group.coordinator_room}@${group.household}`);
          return { household: group.household, coordinator: group.coordinator, queue: { zone: 'Room', done: 'Added selected tracks',
            first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1,
            count: body.source.tracks.length, playing_queue: queue.playing !== null } };
        }) };
  };
  const backend = async (name, args = {}) => {
    calls.push({ name, args: copy(args) });
    const changed = hook?.(name, args); if (changed !== undefined) return changed;
    if (name === 'zones') return rooms ? copy(zones) : [];
    if (name === 'libraryBrowse' || name === 'libraryPlaylist') return catalog(args.query);
    if (name === 'librarySearch') return [
      { kind: 'album', source_uri: album, title: 'Search album' },
      { kind: 'playlist', source_uri: playlist, title: 'Search playlist' },
    ];
    if (name === 'libraryHistory') return [];
    if (name === 'sourceTargetResolve') return resolve(args.query.target);
    if (name === 'sourceQueueState') return queueFor(args.params.room);
    if (name === 'sourceControl') return receipt(args.body);
    if (name === 'queue') return { zone: 'Room', total: 2, revision: state.queueRevision, playing: 1,
      items: [{ position: 1, title: 'Current queue' }] };
    if (name === 'state') return { zone: copy(zones.find(zone => `${zone.coordinator_room}@${zone.household}` === args.params.room)),
      transport_state: 'paused', track: { uri: state.source, queue_position: 1 } };
    if (name === 'libraryPlayTracks') return { done: 'Playing selected tracks', changed: true };
    if (name === 'libraryTracks') return { done: 'Added selected tracks', zone: 'Room', first: args.body.next ? 2 : 3,
      count: args.body.tracks.length, playing_queue: true };
    throw new Error(`Unexpected ${name}`);
  };
  const control = R.model({ call: backend, executeSet: async (groups, work) => {
    state.executions++; state.held = groups; return state.lane ? state.lane(groups, work) : work();
  } }); await control.refreshRooms();
  if (rooms) control.target('Room@S2');
  assert.equal(await control.browse('tracks', 0, collection === 'album' ? { uri: album, title: 'Synthetic album' } : null,
    null, collection === 'playlist' ? { uri: playlist, title: 'Synthetic playlist' } : null), true);
  return { control, calls, state, zones, resolve, receipt, hook(fn) { hook = fn; },
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

test('basket distinguishes collection and filter contexts while keeping labels outside their proof', () => {
  const source = observe(50, { query: { kind: 'tracks', offset: 50, limit: 50, q: '  sonata  ', album_uri: album },
    collectionTitle: 'Sonatas', albumObservation: { coverage: 'complete', fetched_at: 100, cached_total: 112 } });
  const renamed = { ...source, collectionTitle: 'The same album, renamed' };
  assert.equal(Q.contextKey(source), Q.contextKey(renamed)); assert.equal(Q.pageKey(source), Q.pageKey(renamed));
  assert.equal(Q.contextKey(source), Q.contextKey({ ...source, revision: 'new', albumObservation: null }));
  assert.notEqual(Q.pageKey(source), Q.pageKey({ ...source, revision: 'new' }));
  assert.notEqual(Q.pageKey(source), Q.pageKey({ ...source, albumObservation: null }));
  assert.notEqual(Q.contextKey(source), Q.contextKey({ ...source, query: { ...source.query, q: 'concerto' } }));
  assert.equal(Q.originLabel(source), 'Album: Sonatas · Filter: sonata');
  assert.equal(Q.originLabel(observe()), 'Cached tracks');
  assert.equal(Q.originLabel(observe(0, { query: { kind: 'tracks', offset: 0, limit: 50, playlist_uri: playlist },
    collectionTitle: 'Evening' })), 'Playlist: Evening');
  const basket = Q.basket(); basket.add(source, 2, track(53));
  basket.observe(renamed); assert.deepEqual(basket.indices(renamed), [2]);
  assert.equal(basket.add(renamed, 2, track(53)), false, 'renaming a collection is not a repeat request');
});

test('basket keeps first-selected collection order and natural order within each independent context', () => {
  const basket = Q.basket();
  const albumPage = offset => observe(offset, { query: { kind: 'tracks', offset, limit: 50, album_uri: album }, revision: 'album-a' });
  const filtered = observe(0, { query: { kind: 'tracks', offset: 0, limit: 50, q: 'quiet' }, revision: 'filter-b' });
  const playlistPage = observe(0, { query: { kind: 'tracks', offset: 0, limit: 50, playlist_uri: playlist }, revision: 'playlist-c' });
  basket.add(albumPage(50), 2, track(53)); basket.add(filtered, 1, track(2));
  basket.add(playlistPage, 0, track(1)); basket.add(albumPage(0), 0, track(1));
  assert.deepEqual(basket.rows.map(row => row.item.source_uri), [uri(1), uri(53), uri(2), uri(1)]);
  assert.equal(new Set(basket.rows.map(row => Q.contextKey(row.page))).size, 3);
  assert.equal(basket.rows[0].page.revision, 'album-a');
  assert.equal(basket.rows[2].page.revision, 'filter-b');
  const last = basket.rows.at(-1); basket.move(last.id, -1);
  basket.add(albumPage(0), 1, track(2));
  assert.equal(basket.rows.at(-1).page.query.album_uri, album, 'an addition appends after the owner customizes the order');
});

test('same URI selected through different filters is independently retained, removed and bounded', () => {
  const basket = Q.basket(), page = n => observe(0, { query: { kind: 'tracks', offset: 0, limit: 50, q: `filter ${n}` }, revision: `proof-${n}` });
  for (let n = 0; n < 10; n++) basket.add(page(n), 0, track(1));
  assert.equal(new Set(basket.rows.map(row => Q.pageKey(row.page))).size, 10);
  assert.throws(() => basket.add(page(10), 0, track(1)), /at most 10/);
  basket.unselect(page(3), 0); assert.equal(basket.rows.length, 9);
  assert.deepEqual(basket.indices(page(4)), [0]); assert.deepEqual(basket.indices(page(3)), []);
  assert.throws(() => basket.observe({ ...page(4), revision: 'changed' }), /cleared/);
  assert.deepEqual(basket.rows, [], 'one selected context changing retires the whole reviewable selection');
});

for (const change of [
  { revision: 'other' },
  { albumObservation: { coverage: 'complete', fetched_at: 101, cached_total: 112 } },
]) test(`basket retires a changed proof for a selected context: ${JSON.stringify(change)}`, () => {
  const basket = Q.basket(); basket.add(observe(), 0, track(1));
  assert.throws(() => basket.add(observe(50, change), 0, track(51)), /listing changed/);
  assert.equal(basket.rows.length, 0);
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
  assert.match(f.control.state.selectionNotice, /could not be verified.*cleared/); assert.equal(f.writes().length, 0);
});

for (const reset of ['refresh', 'close']) test(`phone ${reset} clears selection and its retained confirmation`, async () => {
  const f = await fixture(); await selectThreePages(f); await f.control.chooseTracks('append'); const pending = f.control.state.pending;
  if (reset === 'refresh') await f.control.refresh();
  else f.control.close();
  assert.deepEqual(f.control.state.basket, []); assert.equal(await f.control.confirm(pending), false);
  assert.match(f.control.state.selectionNotice, /cleared/); assert.equal(f.writes().length, 0);
});

for (const navigation of ['filter draft', 'category', 'search', 'recent']) test(`phone ${navigation} retains selected sources and consumes the old review`, async () => {
  const f = await fixture(); await selectThreePages(f); await f.control.chooseTracks('append'); const pending = f.control.state.pending;
  if (navigation === 'filter draft') f.control.query('new unfinished filter');
  else if (navigation === 'category') assert.equal(await f.control.browse('albums'), true);
  else if (navigation === 'search') { f.control.query('album'); assert.equal(await f.control.search(), true); }
  else assert.equal(await f.control.recent(), true);
  assert.equal(f.control.state.basket.length, 3); assert.equal(await f.control.confirm(pending), false);
  const before = f.reads().length;
  assert.equal(await f.control.chooseTracks('append'), true);
  assert.deepEqual(f.control.state.pending.body.tracks.map(row => row.source_uri), [uri(2), uri(53), uri(102)]);
  assert.ok(f.reads().slice(before).every(read => !read.args.query.q), 'review revalidates the retained filters, not a query draft');
  assert.equal(f.writes().length, 0);
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

function mixedCatalog(query) {
  const page = catalog(query);
  page.revision = JSON.stringify([query.album_uri || query.playlist_uri || 'all', query.q || '']);
  return page;
}

async function selectMixed(f) {
  f.hook((name, args) => ['libraryBrowse', 'libraryPlaylist'].includes(name) ? mixedCatalog(args.query) : undefined);
  assert.equal(await f.control.refresh(), true);
  assert.equal(f.control.selectTrack(1, true), true);
  f.control.query('quiet'); assert.equal(await f.control.filter(), true);
  assert.equal(f.control.selectTrack(0, true), true);
  assert.equal(await f.control.browse('albums'), true); assert.equal(await f.control.openAlbum(0), true);
  assert.equal(f.control.state.query, '', 'opening an album consumes its directory title filter');
  assert.equal(f.control.selectTrack(1, true), true);
  assert.equal(await f.control.browse('playlists'), true); assert.equal(await f.control.openPlaylist(0), true);
  assert.equal(await f.control.next(), true); assert.equal(f.control.selectTrack(0, true), true);
  assert.equal(f.control.state.basket.length, 4);
  return f.control.state.basket;
}

test('phone builds one ordered batch across all tracks, a filter, an album and a playlist', async () => {
  const f = await fixture(), rows = await selectMixed(f);
  assert.deepEqual(rows.map(row => row.item.source), [uri(2), uri(1), uri(2), uri(51)]);
  assert.equal(new Set(rows.map(row => Q.contextKey(row.page))).size, 4);
  assert.equal(new Set(rows.map(row => row.page.revision)).size, 4);
  assert.deepEqual(rows.map(row => Q.originLabel(row.page)), [
    'Cached tracks', 'Cached tracks · Filter: quiet', 'Album: Album 1', 'Playlist: Playlist 1',
  ]);
  assert.equal(f.control.repeatTrack(rows[0].id), true);
  assert.equal(f.control.moveTrack(rows[3].id, -1), true);
  assert.equal(await f.control.browse('albums'), true, 'returning to a directory retains the mixed selection');
  const before = f.reads().length;
  assert.equal(await f.control.chooseTracks('append'), true); const pending = f.control.state.pending;
  assert.deepEqual(pending.body.tracks.map(row => row.source_uri), [uri(2), uri(2), uri(1), uri(51), uri(2)]);
  assert.equal(f.reads().length - before, 4, 'repeats share one source-page read, distinct contexts keep independent reads');
  const reviewed = f.reads().slice(before);
  assert.deepEqual(reviewed.map(call => [call.args.query.album_uri || null, call.args.query.playlist_uri || null,
    call.args.query.q || '', call.args.query.offset]), [[null, null, '', 0], [null, null, 'quiet', 0], [null, playlist, '', 50], [album, null, '', 0]]);
  assert.equal(reviewed[2].args.query.expected_revision, rows[3].page.revision);
  assert.equal(await f.control.confirm(pending), true); assert.equal(f.reads().length - before, 8);
  assert.equal(f.writes().length, 1); assert.deepEqual(f.writes()[0].args.body.tracks, pending.body.tracks);
});

test('phone basket hands an exact mixed-source selection to scene authoring without reading or changing playback', async () => {
  const f = await fixture(), rows = await selectMixed(f);
  assert.equal(f.control.repeatTrack(rows[0].id), true);
  assert.equal(f.control.moveTrack(rows[3].id, -1), true);
  const start = f.calls.length, selected = await f.control.sceneSelection();
  assert.equal(selected.kind, 'tracks');
  assert.deepEqual(selected.tracks.map(track => track.source_uri), [uri(2), uri(2), uri(1), uri(51), uri(2)]);
  assert.deepEqual(selected.group, { target: 'Room@S2', members: ['Room@S2', 'Study@S2'] });
  assert.equal(Object.isFrozen(selected.tracks[0]), true);
  assert.equal(f.control.state.basket.length, 5, 'handoff leaves the original basket editable');
  assert.deepEqual(f.writes(), []);
  assert.ok(f.calls.slice(start).every(call => ['libraryBrowse', 'libraryPlaylist'].includes(call.name)));
  assert.equal(f.control.state.pending, null);
  assert.equal(f.control.trackTarget('trackDestination', 'set'), true);
  assert.equal(await f.control.sceneSelection(), null, 'a saved scene requires an explicit observed group');
});

test('changed library pages and closed reads cannot hand a stale basket to scene authoring', async () => {
  for (const change of ['page', 'close']) {
    const f = await fixture(); await selectThreePages(f); const gate = deferred();
    f.hook((name, args) => {
      if (name !== 'libraryBrowse') return undefined;
      const value = catalog(args.query);
      if (change === 'page') { value.revision = 'changed'; return value; }
      return gate.promise.then(() => value);
    });
    const work = f.control.sceneSelection();
    if (change === 'close') { f.control.close(); gate.resolve(); }
    assert.equal(await work, null); assert.deepEqual(f.writes(), []);
  }
});

test('phone keeps mixed source proofs through search, collection changes and back navigation', async () => {
  const f = await fixture(); await selectMixed(f);
  const retained = f.control.state.basket;
  assert.equal(await f.control.browse('playlists'), true);
  f.control.query('the album'); assert.equal(await f.control.search(), true);
  assert.equal(await f.control.openAlbum(0), true); assert.deepEqual(f.control.state.selected, [1]);
  assert.equal(f.control.state.basket, retained, 'opening an already selected collection does not replace its occurrences');
  assert.equal(await f.control.browse('albums'), true);
  f.control.query('the playlist'); assert.equal(await f.control.search(), true);
  assert.equal(await f.control.openPlaylist(1), true); assert.deepEqual(f.control.state.selected, []);
  assert.equal(await f.control.next(), true); assert.deepEqual(f.control.state.selected, [0]);
  assert.equal(await f.control.previous(), true); assert.equal(f.control.state.basket, retained);
  assert.equal(await f.control.recent(), true); assert.equal(await f.control.chooseTracks('play'), true);
  assert.equal(f.control.state.pending.tracks.rows.length, 4);
});

test('phone retires the entire mixed selection when a selected context changes on return', async () => {
  const f = await fixture(); await selectMixed(f);
  f.hook((name, args) => {
    if (!['libraryBrowse', 'libraryPlaylist'].includes(name)) return;
    const page = mixedCatalog(args.query);
    if (args.query.album_uri === album) page.revision = 'album-changed';
    return page;
  });
  assert.equal(await f.control.browse('albums'), true); assert.equal(f.control.state.basket.length, 4);
  assert.equal(await f.control.openAlbum(0), false); assert.deepEqual(f.control.state.basket, []);
  assert.match(f.control.state.selectionNotice, /could not be verified.*cleared/); assert.equal(f.writes().length, 0);
});

for (const boundary of ['review', 'confirmation']) test(`phone detects a changed album among mixed sources at ${boundary}`, async () => {
  const f = await fixture(); await selectMixed(f);
  if (boundary === 'confirmation') assert.equal(await f.control.chooseTracks('next'), true);
  f.hook((name, args) => {
    if (!['libraryBrowse', 'libraryPlaylist'].includes(name)) return;
    const page = mixedCatalog(args.query);
    if (args.query.album_uri === album) page.album_observation.fetched_at++;
    return page;
  });
  assert.equal(await (boundary === 'review' ? f.control.chooseTracks('next') : f.control.confirm()), false);
  assert.equal(f.control.state.pending, null); assert.equal(f.writes().length, 0);
  assert.match(f.control.state.message, /changed/);
});

test('phone blocks basket edits and review during an unrelated listing read, then keeps the retained sources after failure', async () => {
  const f = await fixture(); f.control.selectTrack(0, true); const row = f.control.state.basket[0], gate = deferred();
  f.hook(name => name === 'libraryBrowse' ? gate.promise : undefined);
  const browsing = f.control.browse('albums');
  assert.equal(await f.control.chooseTracks('append'), false); assert.equal(f.control.repeatTrack(row.id), false);
  gate.resolve({}); assert.equal(await browsing, false);
  assert.equal(f.control.state.basket.length, 1); assert.equal(f.control.state.loading, false);
  f.hook(); assert.equal(f.control.repeatTrack(row.id), true);
  assert.equal(await f.control.chooseTracks('append'), true); assert.equal(f.control.state.pending.body.tracks.length, 2);
});

function uncachedPlaylist(query) {
  return { playlist_uri: query.playlist_uri, name: 'Unread playlist', q: query.q || '',
    offset: query.offset, limit: 50, total: 0, cached_total: 0, examined: 0, source_total: null,
    coverage: 'not_cached', fetched_at: null, revision: null, next_offset: null, items: [] };
}

test('phone can review retained tracks while displaying an unrelated uncached playlist', async () => {
  const f = await fixture(); f.control.selectTrack(0, true);
  f.hook((name, args) => name === 'libraryPlaylist' ? uncachedPlaylist(args.query) : undefined);
  assert.equal(await f.control.browse('tracks', 0, null, null, { uri: playlist, title: 'Unread playlist' }), true);
  assert.equal(f.control.state.page.revision, null); assert.deepEqual(f.control.state.selected, []);
  assert.equal(f.control.state.basket.length, 1); assert.equal(await f.control.chooseTracks('append'), true);
  assert.equal(await f.control.confirm(), true); assert.equal(f.writes().length, 1);
});

test('phone clears all selected sources when their retained playlist becomes unavailable', async () => {
  const f = await fixture('playlist'); f.control.selectTrack(0, true);
  assert.equal(await f.control.browse('tracks'), true); f.control.selectTrack(1, true);
  f.hook((name, args) => name === 'libraryPlaylist' ? uncachedPlaylist(args.query) : undefined);
  assert.equal(await f.control.browse('tracks', 0, null, null, { uri: playlist, title: 'Now unavailable' }), false);
  assert.deepEqual(f.control.state.basket, []); assert.match(f.control.state.selectionNotice, /could not be verified.*cleared/);
  assert.equal(f.writes().length, 0);
});

test('phone validates at most ten distinct selected source pages even across ten filter contexts', async () => {
  const f = await fixture();
  for (let index = 0; index < 10; index++) {
    f.control.query(`filter ${index}`); assert.equal(await f.control.filter(), true);
    assert.equal(f.control.selectTrack(0, true), true);
  }
  const before = f.reads().length;
  f.control.query('unfinished eleventh filter');
  assert.equal(await f.control.chooseTracks('append'), true); assert.equal(f.reads().length - before, 10);
  assert.equal(await f.control.confirm(), true); assert.equal(f.reads().length - before, 20);
  assert.equal(f.writes()[0].args.body.tracks.length, 10);
  assert.ok(f.writes()[0].args.body.tracks.every(track => track.source_uri === uri(1)));
});

for (const action of ['play', 'next', 'append']) test(`phone ${action} sends an ordered mixed-source basket to a reviewed room set once`, async () => {
  for (const target of ['Evening & rooms/#', 'all', 'everywhere', 'all@S1', 'all@S2']) {
    const f = await fixture(), rows = await selectMixed(f);
    f.control.repeatTrack(rows[0].id); f.control.moveTrack(rows[3].id, -1);
    const retained = f.control.state.basket, page = f.control.state.page;
    assert.equal(f.control.trackTarget('trackDestination', 'set'), true);
    assert.equal(f.control.trackTarget('trackTarget', target), true);
    assert.equal(f.control.state.basket, retained); assert.equal(f.control.state.page, page);
    const expected = retained.map(({ item }) => ({ source_uri: item.source, title: item.title }));
    const reads = f.reads().length;
    assert.equal(await f.control.chooseTracks(action), true, f.control.state.message);
    const pending = f.control.state.pending;
    assert.equal(pending.targets.groups.length, target.includes('@') ? 1 : 2);
    assert.deepEqual(pending.body, { zone: target, expected_revision: f.state.targetRevision,
      mode: action, source: { kind: 'tracks', tracks: expected } });
    assert.equal(f.writes().length, 0);
    assert.equal(await f.control.confirm(pending), true, f.control.state.message);
    assert.equal(f.state.executions, 1); assert.equal(f.state.held.length, pending.targets.groups.length);
    assert.equal(f.reads().length - reads, 8, 'all four source contexts are rechecked at review and confirmation');
    assert.deepEqual(f.writes(), [{ name: 'sourceControl', args: { body: pending.body } }]);
    assert.equal(f.control.state.result.complete, true);
    assert.equal(await f.control.confirm(pending), false); assert.equal(f.writes().length, 1);
  }
});

test('phone room-set destination changes preserve the basket and consume the old review', async () => {
  const f = await fixture(); await selectMixed(f);
  f.control.trackTarget('trackDestination', 'set'); f.control.trackTarget('trackTarget', 'all');
  assert.equal(await f.control.chooseTracks('next'), true); const pending = f.control.state.pending, basket = f.control.state.basket;
  f.control.trackTarget('trackTarget', 'all@S1');
  assert.equal(f.control.state.basket, basket); assert.equal(f.control.state.pending, null);
  assert.equal(await f.control.confirm(pending), false); assert.equal(f.writes().length, 0);
  assert.equal(await f.control.chooseTracks('append'), true);
  assert.equal(f.control.state.pending.targets.groups.length, 1);
});

for (const change of ['source', 'identity', 'revision', 'queue']) test(`phone changed ${change} prevents the whole room-set basket write`, async () => {
  const f = await fixture(); await selectMixed(f);
  f.control.trackTarget('trackDestination', 'set'); f.control.trackTarget('trackTarget', 'all');
  assert.equal(await f.control.chooseTracks('play'), true); const pending = f.control.state.pending;
  if (change === 'source') f.hook((name, args) => {
    if (!['libraryBrowse', 'libraryPlaylist'].includes(name)) return;
    const page = mixedCatalog(args.query); if (args.query.playlist_uri) page.items[0].title = 'Changed cached occurrence'; return page;
  });
  if (change === 'identity') f.state.replaced = true;
  if (change === 'revision') f.state.targetRevision = 'b'.repeat(64);
  if (change === 'queue') f.state.queueRevision = 'queue-new';
  assert.equal(await f.control.confirm(pending), false); assert.equal(f.writes().length, 0);
});

test('phone a late room-set resolver cannot restore a review after a target edit', async () => {
  const f = await fixture(), entered = deferred(), reply = deferred(); await selectThreePages(f);
  f.control.trackTarget('trackDestination', 'set'); f.control.trackTarget('trackTarget', 'all');
  f.hook(name => { if (name === 'sourceTargetResolve') { entered.resolve(); return reply.promise; } });
  const reading = f.control.chooseTracks('play'); await entered.promise;
  const basket = f.control.state.basket; f.control.trackTarget('trackTarget', 'all@S2');
  reply.resolve(f.resolve('all')); assert.equal(await reading, false);
  assert.equal(f.control.state.pending, null); assert.equal(f.control.state.basket, basket); assert.equal(f.writes().length, 0);
});

for (const response of ['partial', 'all_failed', 'lost', 'malformed', 'queue_count']) test(`phone retains ${response} room-set results without replaying the selected tracks`, async () => {
  const f = await fixture(); await selectThreePages(f);
  f.control.trackTarget('trackDestination', 'set'); f.control.trackTarget('trackTarget', 'all');
  const action = response === 'queue_count' ? 'next' : 'play';
  assert.equal(await f.control.chooseTracks(action), true); const pending = f.control.state.pending;
  f.hook((name, args) => {
    if (name !== 'sourceControl') return;
    if (response === 'lost') throw new Error('Reply lost');
    const receipt = f.receipt(args.body);
    if (response === 'queue_count') { receipt.added[0].queue.count--; return receipt; }
    if (response === 'malformed') { receipt.batch.results.pop(); return receipt; }
    receipt.batch.complete = false;
    receipt.batch.results[1].result = { status: 'failed', error: { code: 'OUTCOME_UNCERTAIN', detail: 'The reply was lost', retryable: false } };
    if (response === 'all_failed') {
      receipt.batch.results[0].result = { status: 'not_attempted' };
      throw Object.assign(new Error('Playback incomplete'), { batch: receipt.batch });
    }
    return receipt;
  });
  assert.equal(await f.control.confirm(pending), response === 'partial');
  if (['partial', 'all_failed'].includes(response)) {
    assert.equal(f.control.state.result.complete, false);
    assert.match(f.control.state.message, /Room@S1/); assert.match(f.control.state.message, /Room@S2/);
  }
  assert.match(f.control.state.message, /not repeated|Do not repeat/);
  assert.equal(await f.control.confirm(pending), false); assert.equal(f.writes().length, 1);
});
