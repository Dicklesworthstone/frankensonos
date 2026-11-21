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
    fetched_at: 100, revision: 'playlist-a' };
  return { ...page, kind, album_uri: albumUri, coverage: 'cached', revision: 'library-a',
    ...(albumUri ? { album_observation: { coverage: 'complete', fetched_at: 100, cached_total: TOTAL } } : {}) };
}

async function fixture({ collection = 'album', callWrapper } = {}) {
  const group = { household: 'S2', coordinator_room: 'Room', members: ['Room', 'Study'], transport_state: 'paused' };
  const value = { group, playback: { zone: copy(group), transport_state: 'paused', volume: 27,
    track: { uri: 'fixture:current', queue_position: 1 } },
  queue: { zone: 'Room', total: 2, revision: 'queue-a', playing: 1,
    items: [{ position: 1, title: 'Current queue entry' }] } };
  const calls = [];
  let hook;
  const backend = async (url, body) => {
    calls.push({ url, body: copy(body) });
    const override = hook?.(url, body);
    if (override !== undefined) return override;
    if (url === '/zones') return [copy(value.group)];
    if (url.endsWith('/state')) return copy(value.playback);
    if (url.endsWith('/dj')) return { zone: 'Room', running: false, steering: { source: 'none' }, next: [] };
    if (url.includes('/queue?')) return copy(value.queue);
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
  return { value, calls, library, session, controller, select, backend,
    hook(fn) { hook = fn; },
    review: action => session.reviewLibraryTracks(library.chooseTracks(), action, destination()),
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

for (const change of ['filter', 'category', 'refresh', 'close', 'suspend']) {
  test(`${change} clears selections retained from older pages`, async () => {
    const f = await fixture();
    await selectAcrossPages(f);
    if (change === 'filter') f.library.edit('different filter');
    else if (change === 'category') await f.library.browse('playlists');
    else if (change === 'refresh') await f.library.refresh();
    else f.library[change]();
    assert.deepEqual(f.library.state.basket, []);
    assert.deepEqual(f.library.state.selected, []);
    assert.equal(f.library.chooseTracks(), null);
    assert.equal(f.writes().length, 0);
  });
}

test('a late page response after a filter change cannot restore an old selection', async () => {
  const f = await fixture({ collection: 'tracks' }), entered = deferred(), reply = deferred();
  f.select(0);
  let waitingQuery;
  f.hook(url => {
    if (!catalogRead({ url }) || Number(queryOf(url).offset) !== 50) return;
    waitingQuery = queryOf(url); entered.resolve(); return reply.promise;
  });
  const running = f.library.next();
  await entered.promise;
  f.library.edit('new filter');
  reply.resolve(catalog(waitingQuery));
  assert.equal(await running, false);
  assert.equal(f.library.chooseTracks(), null);
  assert.deepEqual(f.library.state.basket, []);
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
