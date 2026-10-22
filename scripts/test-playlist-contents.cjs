'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/playlist-contents.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const R = require('../crates/fsonos-api/src/remote/library.js');
const id = n => String(n).padStart(22, '0');
const playlistUri = `spotify:playlist:${id(77)}`;
const trackUri = n => `spotify:track:${id(n)}`;
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
const group = { household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen'], transport_state: 'paused' };
const queryFrom = url => Object.fromEntries(new URL(url, 'http://localhost').searchParams);

function contents(query = {}, revision = 'list-v1') {
  const offset = Number(query.offset || 0), q = (query.q || '').trim();
  let all = Array.from({ length: 55 }, (_, n) => ({ position: 2 * n + 1,
    source_uri: trackUri([0, 1, 54].includes(n) ? 1 : n + 1), title: [0, 1, 54].includes(n) ? 'Repeat' : `Track ${n}`,
    subtitle: 'Synthetic performer', album: 'An album', album_uri: `spotify:album:${id(9)}`,
    disc_number: 1, track_number: n + 1, duration_secs: 180, explicit: n === 1 }));
  if (q) all = all.filter(item => `${item.title} ${item.subtitle}`.toLowerCase().includes(q.toLowerCase()));
  const items = all.slice(offset, offset + 50);
  return { playlist_uri: playlistUri, name: 'Dinner', q, coverage: 'partial', fetched_at: 1700000000,
    source_total: 180, examined: 110, cached_total: 55, total: all.length, offset, limit: 50,
    next_offset: offset + items.length < all.length ? offset + items.length : null, revision, items };
}

function catalog(query = {}) {
  const kind = query.kind || 'albums', singular = kind.slice(0, -1);
  return { kind, offset: 0, limit: 50, album_uri: null, coverage: 'cached', total: 1,
    next_offset: null, revision: 'catalog-v1', items: [{ kind: singular,
      source_uri: singular === 'playlist' ? playlistUri : `spotify:${singular}:${id(77)}`,
      title: 'Dinner', subtitle: '', explicit: false, cached_tracks: singular === 'album' ? 2 : null }] };
}

test('playlist pages preserve duplicate identities, original gaps and partial coverage', () => {
  const page = P.pageOf(contents(), { playlist_uri: playlistUri });
  assert.equal(page.items.length, 50);
  assert.deepEqual(page.items.slice(0, 3).map(item => item.position), [1, 3, 5]);
  assert.equal(page.items[0].source_uri, page.items[1].source_uri);
  assert.equal(page.items[1].explicit, true);
  assert.equal(page.next, 50);
  assert.match(P.describe(page), /Partial playlist: 110 of 180 source entries read; 55 playable/);
  assert.match(P.describe(page), /55 unavailable or unsupported/);
  assert.throws(() => { page.items[1].position = 2; }, TypeError);
});

test('an unread playlist is distinct from an observed empty one', () => {
  const empty = { ...contents(), coverage: 'complete', source_total: 0, examined: 0, cached_total: 0,
    total: 0, next_offset: null, items: [] };
  assert.match(P.describe(P.pageOf(empty, { playlist_uri: playlistUri })), /read to the end/);
  const unread = { ...empty, coverage: 'not_cached', source_total: null, fetched_at: null, revision: null };
  assert.match(P.describe(P.pageOf(unread, { playlist_uri: playlistUri })), /have not been cached/);
  assert.throws(() => P.pageOf({ ...unread, revision: 'fake-empty' }, { playlist_uri: playlistUri }));
});

test('continuation requires a revision and keeps filters and URI encoded as data', () => {
  assert.throws(() => P.queryOf({ playlist_uri: playlistUri, offset: 50 }));
  assert.throws(() => P.queryOf({ playlist_uri: playlistUri, expected_revision: '' }));
  assert.throws(() => P.queryOf({ playlist_uri: 'https://example.test/list' }));
  const q = P.queryOf({ playlist_uri: playlistUri, offset: 50, q: 'A & B', expected_revision: 'list-v1' });
  assert.equal(L.urlOf({ kind: 'tracks', ...q }), `/library/playlist?playlist_uri=spotify%3Aplaylist%3A${id(77)}&offset=50&limit=50&q=A+%26+B&expected_revision=list-v1`);
});

test('invalid coverage and occurrence positions never authorize music selection', () => {
  for (const change of [p => { p.items[1].position = p.items[0].position; },
    p => { p.items[1].position = 500; }, p => { p.items[1].source_uri = playlistUri; },
    p => { p.items[0].album_uri = 'http://example.test'; }, p => { p.total = 56; },
    p => { p.cached_total = 111; }, p => { p.coverage = 'complete'; },
    p => { p.fetched_at = 'now'; }, p => { p.next_offset = 51; }]) {
    const raw = contents(); change(raw);
    assert.throws(() => P.pageOf(raw, { playlist_uri: playlistUri }));
  }
  assert.throws(() => P.pageOf(contents({ offset: 50 }, 'list-v2'), {
    playlist_uri: playlistUri, offset: 50, expected_revision: 'list-v1' }), /changed between pages/);
});

async function dashboard() {
  const calls = [], state = { revision: 'list-v1', hook: null };
  const call = async (url, body) => {
    calls.push({ url, body: structuredClone(body) });
    if (state.hook) { const value = state.hook(url, body); if (value !== undefined) return value; }
    if (body !== undefined) return url === '/queue'
      ? { done: 'Queued', first: 4, count: 1, playing_queue: false } : { done: 'Playing', changed: true };
    if (url === '/zones') return [structuredClone(group)];
    if (url.endsWith('/state')) return { zone: structuredClone(group), transport_state: 'paused',
      volume: 20, group_volume: 20, track: { uri: 'fixture:playing', queue_position: 1 } };
    if (url.endsWith('/dj')) return { zone: 'Studio', running: false, steering: { source: 'none' }, next: [] };
    if (url.startsWith('/library/playlist?')) return contents(queryFrom(url), state.revision);
    if (url.startsWith('/library/browse?')) return catalog(queryFrom(url));
    throw new Error(`Unexpected route ${url}`);
  };
  const controller = C.model({ call, now: () => 1000 });
  const session = S.model({ call, controller, now: () => 1000 });
  const library = L.model({ call });
  await controller.refresh(); await session.load(); await library.open(); await library.browse('playlists');
  const row = library.state.page.items[0];
  await library.playlist(row);
  const review = (action, item = library.state.page.items[1]) => session.reviewLibrary(library.choose(item), action,
    { target: controller.state.selected, shape: C.shape(controller.selected().zone) });
  return { calls, state, library, controller, session, row, review, writes: () => calls.filter(c => c.body !== undefined) };
}

test('dashboard opens exact playlist identity, pages with a revision and keeps occurrence order', async () => {
  const f = await dashboard();
  assert.equal(f.library.state.playlist.uri, playlistUri);
  assert.equal(f.library.state.page.items[1].position, 3);
  const first = f.library.state.page;
  assert.equal(await f.library.next(), true);
  assert.equal(f.library.state.page.items.at(-1).position, 109);
  assert.equal(queryFrom(f.calls.at(-1).url).expected_revision, 'list-v1');
  assert.equal(await f.library.next(first), false);
  assert.equal(await f.library.playlist(f.row), false);
  await f.library.previous();
  assert.equal(f.library.state.page.items[0].position, 1);
  await f.library.browse('playlists');
  assert.equal(f.library.state.playlist, null);
  assert.equal(f.writes().length, 0);
});

test('playlist filters retain source positions and a changed page retires dashboard choices', async () => {
  const f = await dashboard();
  f.library.edit('Repeat'); await f.library.refresh();
  assert.deepEqual(f.library.state.page.items.map(i => i.position), [1, 3, 109]);
  assert.equal(f.library.state.page.cached_total, 55);
  f.library.edit(''); await f.library.refresh();
  const old = f.library.state.page;
  f.state.revision = 'list-v2';
  assert.equal(await f.library.next(), false);
  assert.equal(f.library.choose(old.items[0]), null);
  assert.match(f.library.state.error, /changed between pages/);
  await f.library.refresh();
  assert.equal(f.library.state.page.revision, 'list-v2');
});

for (const action of ['play', 'append', 'next']) test(`dashboard playlist ${action} uses the exact selected track in the shared writer`, async () => {
  const f = await dashboard(), review = f.review(action);
  assert.equal(review.catalog.item.position, 3);
  assert.match(review.detail, /Playlist position 3/);
  assert.equal(f.writes().length, 0);
  assert.equal(await f.session.confirm(review), true);
  assert.deepEqual(f.writes(), [{ url: action === 'play' ? '/play' : '/queue', body: {
    zone: 'Studio@S2', source_uri: trackUri(1), title: 'Repeat', ...(action === 'play' ? {} : { next: action === 'next' }) } }]);
  assert.equal(await f.session.confirm(review), false);
});

test('playlist revision changes prevent a reviewed dashboard queue action', async () => {
  const f = await dashboard(), review = f.review('next');
  f.state.revision = 'list-v2';
  assert.equal(await f.session.confirm(review), false);
  assert.equal(f.writes().length, 0);
});

test('closing while a playlist page is in flight cannot revive rows', async () => {
  const f = await dashboard(), gate = deferred();
  f.state.hook = url => url.startsWith('/library/playlist?') ? gate.promise : undefined;
  const reading = f.library.refresh(); await tick();
  f.library.close(); gate.resolve(contents());
  assert.equal(await reading, false);
  assert.equal(f.library.choose(f.library.state.page.items[0]), null);
});

test('an uncertain playlist track write is dispatched only once', async () => {
  const f = await dashboard(), review = f.review('next');
  f.state.hook = (url, body) => { if (body !== undefined) throw new Error('Acknowledgement lost'); };
  assert.equal(await f.session.confirm(review), false);
  assert.equal(await f.session.confirm(review), false);
  assert.equal(f.writes().length, 1);
  assert.equal(f.controller.state.uncertain, true);
});

async function remote() {
  const calls = [], state = { revision: 'list-v1' };
  const control = R.model({ call: async (name, args) => {
    calls.push([name, structuredClone(args)]);
    if (name === 'zones') return [structuredClone(group)];
    if (name === 'libraryBrowse') return catalog(args.query);
    if (name === 'libraryPlaylist') return contents(args.query, state.revision);
    if (name === 'libraryAdd') return { done: 'Queued', first: 4, count: 1, playing_queue: false };
    throw new Error(`Unexpected route ${name}`);
  } });
  await control.refreshRooms(); control.target('Studio@S2'); await control.browse('playlists');
  control.query('Dinner'); await control.refresh();
  await control.openPlaylist(0);
  return { calls, state, control, writes: () => calls.filter(([name]) => name === 'libraryAdd') };
}

test('full remote opens a playlist without carrying its title filter into track contents', async () => {
  const f = await remote();
  assert.equal(f.control.state.query, '');
  assert.equal(f.control.state.hits[1].position, 3);
  assert.equal(f.control.state.hits[0].source, f.control.state.hits[1].source);
  await f.control.next();
  assert.equal(f.calls.at(-1)[1].query.expected_revision, 'list-v1');
  assert.equal(f.control.state.hits.at(-1).position, 109);
  assert.deepEqual(R.routes.libraryPlaylist, ['GET', '/library/playlist']);
});

test('full remote rechecks a cached occurrence then queues only its exact track URI', async () => {
  const f = await remote();
  assert.equal(f.control.choose(1, 'next'), true);
  assert.equal(f.control.state.pending.hit.position, 3);
  assert.equal(await f.control.confirm(), true);
  assert.deepEqual(f.writes(), [['libraryAdd', { body: { zone: 'Studio@S2', source_uri: trackUri(1), title: 'Repeat', next: true } }]]);
  assert.equal(f.calls.filter(([name]) => name === 'libraryPlaylist').length, 2);
});

test('a changed cached playlist blocks full-remote writes and invalidates old rows', async () => {
  const f = await remote();
  f.control.choose(1, 'append'); f.state.revision = 'list-v2';
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.writes().length, 0);
  assert.equal(f.control.choose(1, 'append'), false);
  assert.match(f.control.state.message, /changed between pages/);
});
