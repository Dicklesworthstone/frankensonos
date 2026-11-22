'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');

const copy = value => structuredClone(value);
const id = value => String(value).padStart(22, '0');
const uri = value => `spotify:track:${id(value)}`;
const album = `spotify:album:${id(7)}`, playlist = `spotify:playlist:${id(8)}`;
const TOTAL = 123;
const tick = () => new Promise(setImmediate);
const deferred = () => {
  let resolve;
  const promise = new Promise(yes => { resolve = yes; });
  return { promise, resolve };
};
const queryOf = url => Object.fromEntries(new URL(url, 'http://localhost').searchParams);
const catalogRead = call => call.body === undefined && /^\/library\/(browse|playlist)\?/.test(call.url);
const readSources = calls => calls.filter(catalogRead).map(call => {
  const query = queryOf(call.url);
  return [query.album_uri || query.playlist_uri || query.kind, query.q || '', Number(query.offset)];
}).sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));

function catalog(query) {
  const kind = query.kind || 'tracks', offset = Number(query.offset || 0);
  const albumUri = query.album_uri || null, playlistUri = query.playlist_uri || null;
  const total = kind === 'tracks' ? TOTAL : 1;
  const all = Array.from({ length: total }, (_, i) => ({
    kind: kind.slice(0, -1),
    source_uri: kind === 'albums' ? album : kind === 'playlists' ? playlist
      : uri((albumUri || playlistUri) && [50, 100].includes(i) ? 1 : i + 1),
    title: `Track ${i + 1}`, subtitle: 'Synthetic artist', explicit: i === 49,
    ...(kind === 'albums' ? { cached_tracks: TOTAL } : {}),
    ...(albumUri ? { album_uri: albumUri, disc_number: Math.floor(i / 40) + 1,
      track_number: i % 40 + 1, ...(i === 100 ? { explicit_known: false } : {}) } : {}),
    ...(playlistUri ? { position: i + 1 + Math.floor(i / 3) } : {}),
  }));
  const end = Math.min(total, offset + L.LIMIT);
  const page = { offset, limit: L.LIMIT, total, next_offset: end < total ? end : null,
    items: all.slice(offset, end) };
  if (playlistUri) return { ...page, playlist_uri: playlistUri, name: 'Large observed playlist',
    q: query.q || '', cached_total: TOTAL, examined: 170, source_total: 170, coverage: 'complete',
    fetched_at: 100, revision: `playlist-a${query.q ? `:${query.q}` : ''}` };
  return { ...page, kind, album_uri: albumUri, coverage: 'cached',
    revision: `${albumUri ? 'album' : 'library'}-a${query.q ? `:${query.q}` : ''}`,
    ...(albumUri ? { album_observation: { coverage: 'complete', fetched_at: 100, cached_total: TOTAL } } : {}) };
}

function uncachedPlaylist(query) {
  return { ...catalog(query), total: 0, cached_total: 0, examined: 0, source_total: null,
    coverage: 'not_cached', fetched_at: null, revision: null, next_offset: null, items: [] };
}

async function fixture({ collection = 'album', callWrapper, sets = false } = {}) {
  const group = { household: 'S2', coordinator_room: 'Room', members: ['Room', 'Study'], transport_state: 'paused' };
  const value = { group, playback: { zone: copy(group), transport_state: 'paused', volume: 27,
    track: { uri: 'fixture:current', queue_position: 1 } },
  queue: { zone: 'Room', total: 2, revision: 'queue-a', playing: 1,
    items: [{ position: 1, title: 'Current queue entry' }] } };
  const groups = sets ? [group, { ...copy(group), household: 'S1' }] : [group];
  value.targetRevision = 'a'.repeat(64);
  const resolve = target => {
    const selected = groups.filter(zone => !target.startsWith('all@') || zone.household === target.slice(4));
    const resolved = selected.map(zone => {
      const coordinator = `fixture-${zone.household}-Room${value.replaced ? '-new' : ''}`;
      return { household: zone.household, coordinator, coordinator_room: zone.coordinator_room,
        members: zone.members.map(room => ({ household: zone.household, room,
          player: room === 'Room' ? coordinator : `fixture-${zone.household}-${room}`, coordinator })) };
    });
    return { target, revision: value.targetRevision, matched: target.startsWith('all') || target === 'everywhere' ? 'everywhere' : 'alias',
      groups: resolved, rooms: resolved.flatMap(group => group.members) };
  };
  const queueFor = target => target.endsWith('@S1') ? { ...copy(value.queue), total: 4, playing: null,
    revision: value.queue.revision + '-S1' } : copy(value.queue);
  const receipt = body => {
    const selected = resolve(body.zone).groups;
    return body.mode === 'play' ? { done: 'Playing selected tracks', changed: true, ...(selected.length > 1 ? { batch: {
      complete: true, results: selected.map(group => ({ household: group.household, target: group.coordinator, zone: group.coordinator_room,
        result: { status: 'succeeded', done: 'Started selected tracks', changed: true } })) } } : {}) }
      : { done: 'Added selected tracks', complete: true, logged: true, undoable: true, failed: [],
        added: selected.map(group => {
          const queue = queueFor(`${group.coordinator_room}@${group.household}`);
          return { household: group.household, coordinator: group.coordinator, queue: { zone: 'Room', done: 'Added selected tracks',
            first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1,
            count: body.source.tracks.length, playing_queue: queue.playing !== null } };
        }) };
  };
  const calls = [];
  let hook;
  const backend = async (url, body) => {
    calls.push({ url, body: copy(body) });
    const override = hook?.(url, body);
    if (override !== undefined) return override;
    if (url === '/zones') return copy(groups);
    if (url.startsWith('/rooms/resolve?')) return resolve(queryOf(url).target);
    if (url === '/sources/control') return receipt(body);
    if (url.endsWith('/state')) {
      const zone = groups.find(group => url.includes(encodeURIComponent(C.key(group))));
      return { ...copy(value.playback), zone: copy(zone) };
    }
    if (url.endsWith('/dj')) return { zone: 'Room', running: false, steering: { source: 'none' }, next: [] };
    if (url.includes('/queue?')) return queueFor(decodeURIComponent(url.split('/')[2]));
    if (url.startsWith('/library/')) return catalog(queryOf(url));
    if (url === '/queue/tracks') return { done: 'Queued selected tracks', zone: 'Room',
      first: body.next ? 2 : 3, count: body.tracks.length, playing_queue: true };
    if (url === '/play/tracks') return { done: 'Playing selected tracks', changed: true };
    throw new Error(`Unexpected request: ${url}`);
  };
  const call = callWrapper ? callWrapper(backend) : backend;
  const controller = C.model({ call, now: () => 1000 });
  const session = S.model({ call, controller, now: () => 1000 });
  const library = L.model({ call });
  assert.equal(await controller.refresh(), true);
  assert.equal(await session.load(), true);
  assert.equal(await library.open(), true);
  if (collection === 'album') assert.equal(await library.album(library.state.page.items[0]), true);
  else if (collection === 'playlist') {
    assert.equal(await library.browse('playlists'), true);
    assert.equal(await library.playlist(library.state.page.items[0]), true);
  } else assert.equal(await library.browse('tracks'), true);
  const select = index => assert.equal(library.selectTrack(library.state.page.items[index], true), true);
  const destination = () => ({ target: controller.state.selected, shape: C.shape(controller.selected().zone) });
  return { value, groups, calls, library, session, controller, select, backend, resolve, receipt,
    hook(fn) { hook = fn; },
    review: action => session.reviewLibraryTracks(library.chooseTracks(), action, destination()),
    reviewSet: (action, target = 'all') => session.reviewLibraryTracks(library.chooseTracks(), action, { target, set: true }),
    writes: () => calls.filter(call => call.body !== undefined) };
}

async function selectAcrossPages(f) {
  f.select(0);
  assert.equal(await f.library.next(), true);
  f.select(0);
  assert.equal(await f.library.next(), true);
  f.select(0);
  assert.deepEqual(f.library.state.basket.map(row => row.page.query.offset), [0, 50, 100]);
}

async function selectMixedSources(f) {
  f.select(0);
  assert.equal(await f.library.browse('albums'), true);
  assert.equal(await f.library.album(f.library.state.page.items[0]), true);
  assert.equal(await f.library.next(), true);
  f.select(0);
  assert.equal(await f.library.browse('playlists'), true);
  assert.equal(await f.library.playlist(f.library.state.page.items[0]), true);
  assert.equal(await f.library.next(), true);
  assert.equal(await f.library.next(), true);
  f.select(0);
  assert.deepEqual(f.library.state.basket.map(row => row.page.revision), ['library-a', 'album-a', 'playlist-a']);
  assert.deepEqual(f.library.state.basket.map(row => row.item.source_uri), [uri(1), uri(1), uri(1)]);
  assert.equal(new Set(f.library.state.basket.map(row => row.id)).size, 3,
    'the same song selected from three source occurrences remains three entries');
}

const mixedSources = [[album, '', 50], [playlist, '', 100], ['tracks', '', 0]];

for (const action of ['play', 'next', 'append']) {
  test(`tracks, album and playlist occurrences compose one reordered ${action} command with distinct revisions`, async () => {
    const f = await fixture({ collection: 'tracks' });
    await selectMixedSources(f);
    const [track, albumTrack, playlistTrack] = f.library.state.basket;
    assert.equal(f.library.repeatTrack(track.id), true);
    assert.equal(f.library.moveTrack(playlistTrack.id, -1), true);
    assert.deepEqual(f.library.state.basket.map(row => row.item.title), ['Track 1', 'Track 1', 'Track 101', 'Track 51']);
    assert.deepEqual(f.library.state.selected, [0]);
    const expected = f.library.chooseTracks().rows.map(row => ({ source_uri: row.item.source_uri, title: row.item.title }));
    const preparing = f.calls.length;
    const review = await f.review(action);
    assert.ok(review);
    assert.deepEqual(readSources(f.calls.slice(preparing)), mixedSources,
      'preparation reads each distinct source page once, including offscreen sources and repeats');
    assert.deepEqual(review.body.tracks, expected);
    assert.match(review.trackSelection[0], /Cached tracks/);
    assert.match(review.trackSelection[2], /Playlist:.*playlist position|playlist position.*Playlist:/);
    assert.match(review.trackSelection[3], /disc 2, track 11.*Album:/);
    assert.equal(f.writes().length, 0);
    const confirming = f.calls.length;
    assert.equal(await f.session.confirm(review), true);
    assert.deepEqual(readSources(f.calls.slice(confirming)), mixedSources,
      'confirmation freshly rechecks all three source pages before sending any mutation');
    const playlistRead = f.calls.slice(confirming).find(call => catalogRead(call) && queryOf(call.url).playlist_uri);
    assert.equal(queryOf(playlistRead.url).expected_revision, playlistTrack.page.revision);
    assert.equal(f.writes().length, 1);
    assert.equal(f.writes()[0].url, action === 'play' ? '/play/tracks' : '/queue/tracks');
    assert.deepEqual(f.writes()[0].body.tracks, expected);
    assert.equal(f.writes()[0].body.next, action === 'play' ? undefined : action === 'next');
    assert.equal(f.writes()[0].body.expected_revision, 'queue-a');
    assert.equal(albumTrack.page.query.album_uri, album);
  });
}

test('filtered and unfiltered occurrences retain independent revisions and checkbox identities', async () => {
  const f = await fixture({ collection: 'tracks' });
  f.select(0);
  const first = f.library.state.basket[0];
  assert.equal(f.library.edit('different filter'), true);
  assert.equal(f.library.chooseTracks().rows[0].page.query.q, '', 'an unapplied draft cannot change retained provenance');
  assert.equal(await f.library.applyFilter(), true);
  assert.deepEqual(f.library.state.selected, []);
  f.select(0);
  assert.deepEqual(f.library.state.basket.map(row => row.item.source_uri), [uri(1), uri(1)]);
  assert.deepEqual(f.library.state.basket.map(row => row.page.revision), ['library-a', 'library-a:different filter']);
  assert.notEqual(f.library.state.basket[1].id, first.id);
  assert.equal(f.library.edit(''), true);
  assert.equal(await f.library.applyFilter(), true);
  assert.deepEqual(f.library.state.selected, [0]);
  assert.equal(f.library.selectTrack(f.library.state.page.items[0], false), true);
  assert.equal(f.library.state.basket.length, 1);
  assert.equal(f.library.state.basket[0].page.query.q, 'different filter');
  f.select(1);
  const expectedReads = [['tracks', '', 0], ['tracks', 'different filter', 0]];
  const preparing = f.calls.length, review = await f.review('append');
  assert.ok(review);
  assert.deepEqual(readSources(f.calls.slice(preparing)), expectedReads);
  assert.match(review.trackSelection[0], /Filter: different filter/);
  const confirming = f.calls.length;
  assert.equal(await f.session.confirm(review), true);
  assert.deepEqual(readSources(f.calls.slice(confirming)), expectedReads);
  assert.deepEqual(f.writes()[0].body.tracks.map(row => row.source_uri), [uri(1), uri(2)]);
});

test('mixed source revisions are valid, but conflicting revisions in one source cannot form a review', async () => {
  const f = await fixture({ collection: 'tracks' });
  await selectMixedSources(f);
  const selected = f.library.chooseTracks();
  assert.equal(L.selectionOf(selected).rows.length, 3);
  const conflict = copy(selected.rows[0]);
  conflict.page.query.offset = 50;
  conflict.page.revision = 'a-newer-track-listing';
  const invalid = { rows: [...selected.rows, conflict] };
  assert.throws(() => L.selectionOf(invalid), /selected library pages changed/);
  const before = f.calls.length;
  assert.equal(await f.session.reviewLibraryTracks(invalid, 'append', {
    target: f.controller.state.selected, shape: C.shape(f.controller.selected().zone),
  }), null);
  assert.equal(f.session.state.pending, null);
  assert.equal(f.calls.length, before, 'inconsistent source revisions are refused before any I/O');
  assert.equal(f.writes().length, 0);
});

for (const phase of ['preparation', 'confirmation']) for (const source of ['tracks', 'album']) {
  test(`a changed offscreen ${source} source blocks the whole mixed batch during ${phase}`, async () => {
    const f = await fixture({ collection: 'tracks' });
    await selectMixedSources(f);
    const review = phase === 'confirmation' ? await f.review('next') : null;
    if (phase === 'confirmation') assert.ok(review);
    f.hook(url => {
      if (!catalogRead({ url })) return;
      const query = queryOf(url), matches = source === 'album' ? !!query.album_uri : !query.album_uri && !query.playlist_uri;
      if (!matches) return;
      return { ...catalog(query), revision: 'a-changed-offscreen-observation' };
    });
    assert.equal(f.library.state.page.query.playlist_uri, playlist, 'the changed source is no longer onscreen');
    if (phase === 'confirmation') assert.equal(await f.session.confirm(review), false);
    else assert.equal(await f.review('next'), null);
    assert.equal(f.session.state.pending, null);
    assert.equal(f.writes().length, 0, 'no partial batch may dispatch after another source checked successfully');
    assert.match(phase === 'confirmation' ? f.controller.state.message : f.session.state.error, /changed/);
  });
}

for (const change of ['revision', 'observation', 'metadata']) {
  test(`returning to an album with changed ${change} retires selections from every source`, async () => {
    const f = await fixture({ collection: 'tracks' });
    await selectMixedSources(f);
    assert.equal(await f.library.browse('albums'), true);
    f.hook(url => {
      if (!catalogRead({ url }) || !queryOf(url).album_uri) return;
      const page = catalog(queryOf(url));
      if (change === 'revision') page.revision = 'album-b';
      if (change === 'observation') page.album_observation.fetched_at++;
      if (change === 'metadata' && page.offset === 50) page.items[0].title = 'A changed selected occurrence';
      return page;
    });
    const opened = await f.library.album(f.library.state.page.items[0]);
    if (change === 'metadata') {
      assert.equal(opened, true, 'an unselected album page cannot prove an offscreen row changed');
      assert.equal(await f.library.next(), false);
    } else assert.equal(opened, false);
    assert.deepEqual(f.library.state.basket, []);
    assert.equal(f.library.chooseTracks(), null);
    assert.match(f.library.state.error, /changed/);
    assert.equal(f.writes().length, 0);
  });
}

test('an unrelated uncached playlist preserves a reviewable basket and its original source proof', async () => {
  const f = await fixture({ collection: 'tracks' });
  f.select(0);
  const retained = f.library.state.basket;
  assert.equal(await f.library.browse('playlists'), true);
  f.hook(url => {
    if (catalogRead({ url }) && queryOf(url).playlist_uri) return uncachedPlaylist(queryOf(url));
  });
  assert.equal(await f.library.playlist(f.library.state.page.items[0]), true);
  assert.equal(f.library.state.page.coverage, 'not_cached');
  assert.equal(f.library.state.page.revision, null);
  assert.match(L.playlistCoverage(f.library.state.page), /have not been cached/);
  assert.equal(f.library.state.basket, retained);
  assert.deepEqual(f.library.state.selected, []);
  assert.equal(f.library.chooseTracks().rows.length, 1);
  const preparing = f.calls.length, review = await f.review('append');
  assert.ok(review);
  assert.deepEqual(readSources(f.calls.slice(preparing)), [['tracks', '', 0]]);
  const confirming = f.calls.length;
  assert.equal(await f.session.confirm(review), true);
  assert.deepEqual(readSources(f.calls.slice(confirming)), [['tracks', '', 0]]);
  assert.deepEqual(f.writes()[0].body.tracks, [{ source_uri: uri(1), title: 'Track 1' }]);
});

test('a selected playlist becoming uncached clears the whole mixed-source basket', async () => {
  const f = await fixture({ collection: 'tracks' });
  await selectMixedSources(f);
  assert.equal(await f.library.browse('playlists'), true);
  f.hook(url => {
    if (catalogRead({ url }) && queryOf(url).playlist_uri) return uncachedPlaylist(queryOf(url));
  });
  assert.equal(await f.library.playlist(f.library.state.page.items[0]), false);
  assert.deepEqual(f.library.state.basket, []);
  assert.deepEqual(f.library.state.selected, []);
  assert.equal(f.library.chooseTracks(), null);
  assert.equal(f.library.state.resetRequired, true);
  assert.match(f.library.state.error, /selected playlist.*no longer available/);
  assert.match(f.library.state.selectionNotice, /no longer cached.*cleared/);
  assert.equal(f.writes().length, 0);
});

test('failure to read an unrelated album leaves the observed basket reviewable', async () => {
  const f = await fixture({ collection: 'tracks' });
  f.select(0);
  const retained = f.library.state.basket;
  assert.equal(await f.library.browse('albums'), true);
  f.hook(url => {
    if (catalogRead({ url }) && queryOf(url).album_uri) throw new Error('Unrelated album unavailable');
  });
  assert.equal(await f.library.album(f.library.state.page.items[0]), false);
  assert.equal(f.library.state.basket, retained);
  assert.equal(f.library.state.stale, true);
  assert.match(f.library.state.error, /Unrelated album unavailable/);
  assert.equal(f.library.chooseTracks().rows.length, 1);
  const preparing = f.calls.length, review = await f.review('next');
  assert.ok(review);
  assert.deepEqual(readSources(f.calls.slice(preparing)), [['tracks', '', 0]]);
  const confirming = f.calls.length;
  assert.equal(await f.session.confirm(review), true);
  assert.deepEqual(readSources(f.calls.slice(confirming)), [['tracks', '', 0]]);
  assert.equal(f.writes().length, 1);
  assert.deepEqual(f.writes()[0].body.tracks, [{ source_uri: uri(1), title: 'Track 1' }]);
});

for (const collection of ['tracks', 'album', 'playlist']) for (const action of ['play', 'next', 'append']) {
  test(`${collection}: reordered, repeated selections cross three pages in one ${action} command`, async () => {
    const f = await fixture({ collection });
    await selectAcrossPages(f);
    const original = f.library.state.basket;
    assert.equal(f.library.repeatTrack(original[0].id), true);
    assert.equal(f.library.moveTrack(original[2].id, -1), true);
    assert.equal(await f.library.previous(), true);
    assert.equal(await f.library.previous(), true);
    assert.deepEqual(f.library.state.selected, [0]);
    f.select(1);
    const selected = f.library.chooseTracks();
    assert.deepEqual(selected.rows.map(row => row.page.query.offset + row.index), [0, 0, 100, 50, 1]);
    const expected = selected.rows.map(row => ({ source_uri: row.item.source_uri, title: row.item.title }));
    assert.equal(expected.length, 5);
    const preparing = f.calls.length;
    const review = await f.review(action);
    assert.ok(review);
    assert.deepEqual(f.calls.slice(preparing).filter(catalogRead).map(call => Number(queryOf(call.url).offset))
      .sort((a, b) => a - b), [0, 50, 100], 'the review starts from one fresh observation of every selected page');
    assert.deepEqual(review.body.tracks, expected);
    assert.equal(review.body.expected_revision, 'queue-a');
    assert.equal(review.body.zone, 'Room@S2');
    assert.equal(f.writes().length, 0);
    if (collection === 'album') {
      assert.match(review.trackSelection[2], /disc 3, track 21/);
      assert.match(review.trackSelection[2], /explicit metadata unavailable/);
    }
    if (collection === 'playlist') assert.match(review.trackSelection[3], /playlist position 67/);
    const before = f.calls.length;
    assert.equal(await f.session.confirm(review), true);
    assert.equal(await f.session.confirm(review), false);
    const reads = f.calls.slice(before).filter(catalogRead);
    assert.deepEqual(reads.map(call => Number(queryOf(call.url).offset)).sort((a, b) => a - b), [0, 50, 100],
      'repeat copies and multiple rows on a page share one fresh page read');
    if (collection === 'playlist') {
      assert.ok(reads.every(call => queryOf(call.url).expected_revision === 'playlist-a'));
    } else assert.ok(reads.every(call => !Object.hasOwn(queryOf(call.url), 'expected_revision')));
    assert.equal(f.writes().length, 1);
    assert.equal(f.writes()[0].url, action === 'play' ? '/play/tracks' : '/queue/tracks');
    assert.deepEqual(f.writes()[0].body.tracks, expected);
    assert.equal(f.writes()[0].body.next, action === 'play' ? undefined : action === 'next');
  });
}

test('basket operations remove exact repeated occurrences and enforce the ten-entry bound across pages', async () => {
  const f = await fixture({ collection: 'tracks' });
  await selectAcrossPages(f);
  const [first, second, third] = f.library.state.basket;
  assert.equal(f.library.repeatTrack(second.id), true);
  const repeated = f.library.state.basket.find(row => ![first.id, second.id, third.id].includes(row.id));
  assert.ok(repeated);
  assert.equal(f.library.removeTrack(second.id), true);
  assert.ok(f.library.state.basket.some(row => row.id === repeated.id));
  assert.equal(f.library.removeTrack(second.id), false);
  while (f.library.state.basket.length < 10) assert.equal(f.library.repeatTrack(first.id), true);
  const before = f.library.state.basket;
  assert.equal(f.library.repeatTrack(first.id), false);
  assert.equal(f.library.selectTrack(f.library.state.page.items[1], true), false);
  assert.equal(f.library.state.basket, before);
  assert.equal(f.library.chooseTracks().rows.length, 10);
  assert.match(f.library.state.error, /at most 10/);
  assert.equal(f.writes().length, 0);
});

for (const collection of ['tracks', 'album', 'playlist']) {
  test(`${collection}: a changed continuation revision retires the entire basket`, async () => {
    const f = await fixture({ collection });
    f.select(0);
    f.hook(url => {
      if (!catalogRead({ url })) return;
      return { ...catalog(queryOf(url)), revision: 'different-observation' };
    });
    assert.equal(await f.library.next(), false);
    assert.equal(f.library.chooseTracks(), null);
    assert.deepEqual(f.library.state.basket, []);
    assert.match(f.library.state.error, /changed/);
    assert.equal(f.writes().length, 0);
  });
}

for (const change of ['revision', 'title', 'positions', 'coverage', 'missing_page', 'failure']) {
  test(`a ${change} change on a later selected page refuses the entire reviewed batch`, async () => {
    const collection = change === 'positions' ? 'playlist' : 'album';
    const f = await fixture({ collection });
    await selectAcrossPages(f);
    const review = await f.review('play');
    assert.ok(review);
    f.hook(url => {
      if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 50) return;
      if (change === 'failure') throw new Error('Selected page unavailable');
      const page = catalog(queryOf(url));
      if (change === 'revision') page.revision = 'library-b';
      if (change === 'title') page.items[0].title = 'A different observed track';
      if (change === 'positions') page.items[0].position++;
      if (change === 'coverage') page.album_observation.fetched_at++;
      if (change === 'missing_page') page.items = [];
      return page;
    });
    const before = f.calls.length;
    assert.equal(await f.session.confirm(review), false);
    assert.equal(await f.session.confirm(review), false);
    assert.equal(f.writes().length, 0, 'a successful first page never permits a partial submission');
    assert.deepEqual(f.calls.slice(before).filter(catalogRead).map(call => Number(queryOf(call.url).offset)), [0, 50]);
    assert.match(f.controller.state.message, /changed|incomplete|unavailable/);
  });
}

for (const stop of ['cancel', 'suspend']) {
  test(`${stop} while checking a second selected page prevents dispatch and later page reads`, async () => {
    const f = await fixture();
    await selectAcrossPages(f);
    const review = await f.review('next'), entered = deferred(), reply = deferred();
    let waitingQuery;
    f.hook(url => {
      if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 50) return;
      waitingQuery = queryOf(url); entered.resolve(); return reply.promise;
    });
    const before = f.calls.length, running = f.session.confirm(review);
    await entered.promise;
    f.session[stop]();
    reply.resolve(catalog(waitingQuery));
    assert.equal(await running, false);
    assert.equal(f.writes().length, 0);
    assert.deepEqual(f.calls.slice(before).filter(catalogRead).map(call => Number(queryOf(call.url).offset)), [0, 50]);
    assert.equal(f.session.state.pending, null);
    assert.equal(await f.session.confirm(review), false);
  });
}

for (const stop of ['cancel', 'suspend']) {
  test(`${stop} during preparation of a later selected page cannot publish a stale review`, async () => {
    const f = await fixture(), entered = deferred(), reply = deferred();
    await selectAcrossPages(f);
    let waitingQuery;
    f.hook(url => {
      if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 50) return;
      waitingQuery = queryOf(url); entered.resolve(); return reply.promise;
    });
    const before = f.calls.length, preparing = f.review('play');
    await entered.promise;
    f.session[stop]();
    reply.resolve(catalog(waitingQuery));
    assert.equal(await preparing, null);
    assert.equal(f.session.state.pending, null);
    assert.equal(f.writes().length, 0);
    assert.deepEqual(f.calls.slice(before).filter(catalogRead).map(call => Number(queryOf(call.url).offset)), [0, 50]);
  });
}

test('a changed selected row on a revisited page retires the basket even with a reused revision', async () => {
  const f = await fixture();
  f.select(0);
  assert.equal(await f.library.next(), true);
  f.select(0);
  f.hook(url => {
    if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 0) return;
    const page = catalog(queryOf(url));
    page.items[0].title = 'An inconsistent old-page observation';
    return page;
  });
  assert.equal(await f.library.previous(), false);
  assert.equal(f.library.chooseTracks(), null);
  assert.deepEqual(f.library.state.basket, []);
  assert.match(f.library.state.error, /changed/);
  assert.equal(f.writes().length, 0);
});

for (const change of ['refresh', 'close', 'suspend']) {
  test(`${change} clears selections retained from older pages`, async () => {
    const f = await fixture();
    await selectAcrossPages(f);
    if (change === 'refresh') await f.library.refresh();
    else f.library[change]();
    assert.deepEqual(f.library.state.basket, []);
    assert.deepEqual(f.library.state.selected, []);
    assert.equal(f.library.chooseTracks(), null);
    assert.equal(f.writes().length, 0);
  });
}

test('an unapplied filter retains a reviewable basket but cannot select stale displayed rows', async () => {
  const f = await fixture();
  await selectAcrossPages(f);
  const retained = f.library.state.basket, staleItem = f.library.state.page.items[1];
  assert.equal(f.library.edit('a draft not sent to the cache'), true);
  assert.equal(f.library.state.basket, retained);
  assert.deepEqual(f.library.state.selected, []);
  assert.equal(f.library.selectTrack(staleItem, true), false);
  assert.equal(f.library.chooseTracks().rows.length, 3);
  const preparing = f.calls.length, review = await f.review('append');
  assert.ok(review);
  const reads = f.calls.slice(preparing).filter(catalogRead);
  assert.equal(reads.length, 3);
  assert.ok(reads.every(call => !queryOf(call.url).q), 'only original source filters may be verified or submitted');
  assert.equal(f.writes().length, 0);
});

for (const change of ['filter', 'category']) {
  test(`a pending ${change} read retains earlier pages while blocking basket review until completion`, async () => {
    const f = await fixture(), entered = deferred(), reply = deferred();
    await selectAcrossPages(f);
    const retained = f.library.state.basket;
    let waitingQuery;
    f.hook(url => {
      if (!catalogRead({ url })) return;
      waitingQuery = queryOf(url); entered.resolve(); return reply.promise;
    });
    if (change === 'filter') assert.equal(f.library.edit('different filter'), true);
    const running = change === 'filter' ? f.library.applyFilter() : f.library.browse('playlists');
    await entered.promise;
    assert.equal(f.library.state.basket, retained);
    assert.deepEqual(f.library.state.selected, []);
    assert.equal(f.library.chooseTracks(), null);
    assert.equal(f.library.repeatTrack(retained[0].id), false);
    assert.equal(await f.review('append'), null);
    reply.resolve(catalog(waitingQuery));
    assert.equal(await running, true);
    assert.equal(f.library.state.basket, retained);
    assert.deepEqual(f.library.state.selected, []);
    assert.equal(f.library.chooseTracks().rows.length, 3);
    assert.equal(f.writes().length, 0);
  });
}

test('a late page response after a filter edit cannot adopt that page or rewrite retained sources', async () => {
  const f = await fixture({ collection: 'tracks' }), entered = deferred(), reply = deferred();
  f.select(0);
  const retained = f.library.state.basket, originalPage = f.library.state.page;
  let waitingQuery;
  f.hook(url => {
    if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 50) return;
    waitingQuery = queryOf(url); entered.resolve(); return reply.promise;
  });
  const running = f.library.next();
  await entered.promise;
  f.library.edit('new filter');
  assert.equal(f.library.chooseTracks(), null, 'the old outstanding read keeps review blocked');
  reply.resolve(catalog(waitingQuery));
  assert.equal(await running, false);
  assert.equal(f.library.state.page, originalPage);
  assert.equal(f.library.state.filter, 'new filter');
  assert.equal(f.library.state.basket, retained);
  assert.equal(f.library.chooseTracks().rows[0].page.query.q, '');
  assert.equal(f.library.chooseTracks().rows[0].page.query.offset, 0);
  assert.equal(f.library.selectTrack(originalPage.items[1], true), false);
  await tick();
  assert.equal(f.writes().length, 0);
});

test('a cross-page selection with a lost HTTP acknowledgement is submitted once and never replayed', async t => {
  let backend, posts = 0;
  const server = http.createServer(async (req, res) => {
    try {
      let text = '';
      for await (const chunk of req) text += chunk;
      const result = await backend(req.url, text ? JSON.parse(text) : undefined);
      if (req.method === 'POST') { posts++; req.socket.destroy(); return; }
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify(result));
    } catch (error) {
      res.statusCode = 500; res.end(JSON.stringify({ detail: error.message }));
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await fixture({ collection: 'playlist', callWrapper: raw => {
    backend = raw;
    return C.request({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) });
  } });
  await selectAcrossPages(f);
  const review = await f.review('append');
  assert.ok(review);
  assert.equal(await f.session.confirm(review), false);
  assert.equal(await f.session.confirm(review), false);
  assert.equal(posts, 1);
  assert.equal(f.writes().length, 1);
  assert.deepEqual(f.writes()[0].body.tracks.map(track => track.source_uri), [uri(1), uri(1), uri(1)]);
  assert.deepEqual(f.writes()[0].body.tracks.map(track => track.title), ['Track 1', 'Track 51', 'Track 101']);
  assert.equal(f.controller.state.uncertain, true);
  await f.controller.refresh();
  assert.equal(await f.review('append'), null, 'ordinary background refresh cannot authorize retry');
  assert.equal(posts, 1);
});

for (const action of ['play', 'next', 'append']) test(`dashboard ${action} sends a mixed-source ordered basket to each reviewed group once`, async () => {
  for (const target of ['Evening & rooms/#', 'all', 'everywhere', 'all@S1', 'all@S2']) {
    const f = await fixture({ collection: 'tracks', sets: true }); await selectMixedSources(f);
    const rows = f.library.state.basket; f.library.repeatTrack(rows[0].id); f.library.moveTrack(rows[2].id, -1);
    const expected = f.library.state.basket.map(({ item }) => ({ source_uri: item.source_uri, title: item.title }));
    const reads = f.calls.filter(catalogRead).length;
    const review = await f.reviewSet(action, target); assert.ok(review, f.session.state.error);
    assert.equal(review.target, 'Room@S2', 'the controller anchor remains separate from the selected room-set query');
    assert.equal(review.targets.target, target);
    assert.equal(review.targets.groups.length, target.includes('@') ? 1 : 2);
    assert.deepEqual(review.body, { zone: target, expected_revision: f.value.targetRevision,
      mode: action, source: { kind: 'tracks', tracks: expected } });
    assert.equal(review.targetGroups.length, review.targets.groups.length);
    assert.equal(f.writes().length, 0);
    assert.equal(await f.session.confirm(review), true, f.controller.state.message);
    assert.equal(f.calls.filter(catalogRead).length - reads, 6, 'all selected source pages are checked twice');
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sources/control', body: review.body }]);
    assert.equal(f.session.state.result.complete, true);
    assert.equal(await f.session.confirm(review), false); assert.equal(f.writes().length, 1);
  }
});

test('dashboard changing the reviewed room set retains the basket and retires the prior confirmation', async () => {
  const f = await fixture({ collection: 'tracks', sets: true }); await selectMixedSources(f);
  const basket = f.library.state.basket, old = await f.reviewSet('play', 'all'); assert.ok(old);
  f.session.cancel();
  const next = await f.reviewSet('append', 'all@S1'); assert.ok(next);
  assert.equal(f.library.state.basket, basket); assert.equal(next.targets.groups.length, 1);
  assert.equal(await f.session.confirm(old), false); assert.equal(f.writes().length, 0);
});

for (const change of ['source', 'identity', 'revision', 'queue']) test(`dashboard changed ${change} prevents the whole reviewed room-set basket write`, async () => {
  const f = await fixture({ collection: 'tracks', sets: true }); await selectMixedSources(f);
  const review = await f.reviewSet('play'); assert.ok(review);
  if (change === 'source') f.hook(url => {
    if (!catalogRead({ url })) return;
    const query = queryOf(url), page = catalog(query);
    if (query.playlist_uri) page.items[0].title = 'Changed cached occurrence'; return page;
  });
  if (change === 'identity') f.value.replaced = true;
  if (change === 'revision') f.value.targetRevision = 'b'.repeat(64);
  if (change === 'queue') f.value.queue.revision = 'queue-new';
  assert.equal(await f.session.confirm(review), false); assert.equal(f.writes().length, 0);
});

test('dashboard cancelling a pending room-set resolution cannot publish a late basket review', async () => {
  const f = await fixture({ sets: true }), entered = deferred(), reply = deferred(); await selectAcrossPages(f);
  f.hook(url => { if (url.startsWith('/rooms/resolve?')) { entered.resolve(); return reply.promise; } });
  const preparing = f.reviewSet('play'); await entered.promise; f.session.cancel();
  reply.resolve(f.resolve('all')); assert.equal(await preparing, null);
  assert.equal(f.session.state.pending, null); assert.equal(f.writes().length, 0);
});

test('dashboard room-set basket holds its existing command lane until the one receipt arrives', async () => {
  const f = await fixture({ sets: true }), reply = deferred(); await selectAcrossPages(f);
  const review = await f.reviewSet('next');
  f.hook((url, body) => url === '/sources/control' ? reply.promise : undefined);
  const sending = f.session.confirm(review); while (!f.writes().length) await tick();
  assert.equal(f.controller.state.busy, true); assert.equal(await f.controller.command('resume'), false);
  assert.equal(await f.session.confirm(review), false);
  reply.resolve(f.receipt(review.body)); assert.equal(await sending, true); assert.equal(f.writes().length, 1);
});

for (const response of ['partial', 'all_failed', 'lost', 'malformed', 'queue_count']) test(`dashboard retains ${response} room-set results without replaying the selected tracks`, async () => {
  const f = await fixture({ sets: true }); await selectAcrossPages(f);
  const review = await f.reviewSet(response === 'queue_count' ? 'next' : 'play'); assert.ok(review);
  f.hook((url, body) => {
    if (url !== '/sources/control') return;
    if (response === 'lost') throw new Error('Reply lost');
    const receipt = f.receipt(body);
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
  assert.equal(await f.session.confirm(review), response === 'partial');
  if (['partial', 'all_failed'].includes(response)) {
    assert.equal(f.session.state.result.complete, false);
    assert.match(f.session.state.result.detail, /Room@S1/); assert.match(f.session.state.result.detail, /Room@S2/);
  }
  assert.match(f.controller.state.message, /no retry|not repeated|Do not repeat/);
  assert.equal(await f.session.confirm(review), false); assert.equal(f.writes().length, 1);
});

test('a queue event in any reviewed group retires the dashboard basket review', async () => {
  const f = await fixture({ sets: true }); await selectAcrossPages(f);
  const review = await f.reviewSet('next'); assert.ok(review);
  f.session.queueChanged('Room@S1');
  assert.equal(f.session.state.pending, null); assert.equal(await f.session.confirm(review), false);
  assert.equal(f.writes().length, 0);
});
