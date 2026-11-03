'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const { model, routes, LIMIT, MAX_QUERY } = require('../crates/fsonos-api/src/remote/library.js');

const zones = [
  { household: 'S1', coordinator_room: 'Room', members: ['Room', 'Study'] },
  { household: 'S2', coordinator_room: 'Room', members: ['Room'] },
];
const item = (n, kind = 'track', changes = {}) => ({ kind, source_uri: `spotify:${kind}:Item${n}`,
  title: `Title ${n}`, subtitle: 'Synthetic Artist', album: 'Album', album_uri: 'spotify:album:Album1',
  explicit: false, cached_tracks: kind === 'album' ? 3 : null, disc_number: 1, track_number: n + 1, ...changes });
function page(query = {}, changes = {}) {
  const kind = query.kind || 'tracks';
  const total = changes.total ?? 117;
  const offset = query.offset || 0;
  const count = Math.min(LIMIT, Math.max(0, total - offset));
  return { kind, offset, limit: LIMIT, total, next_offset: offset + count < total ? offset + count : null,
    revision: 'cache-example', coverage: 'cached', album_uri: query.album_uri || null,
    items: Array.from({ length: count }, (_, i) => item(offset + i, kind.slice(0, -1),
      { album_uri: query.album_uri || 'spotify:album:Album1' })), ...changes };
}
const later = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
function fixture(override = async () => undefined) {
  const calls = [];
  const control = model({ call: async (name, args) => {
    calls.push([name, args]);
    const value = await override(name, args);
    if (value !== undefined) return value;
    if (name === 'zones') return structuredClone(zones);
    if (name === 'libraryBrowse') return page(args.query);
    if (name === 'librarySearch') return [item(1)];
    if (name === 'libraryHistory') return [];
    if (name === 'libraryAdd') return { done: 'Queued', first: 2, count: 1, playing_queue: false };
    return { done: 'Playing', changed: true };
  } });
  return { control, calls, writes: () => calls.filter(([name]) => ['libraryPlay', 'libraryAdd', 'playFavorite'].includes(name)) };
}
async function open(f, target = false) {
  await f.control.refreshRooms();
  if (target) f.control.target('Room@S2');
}

test('browse routes are read-only and require no selected destination or search words', async () => {
  const f = fixture();
  assert.equal(f.calls.length, 0);
  await open(f);
  assert.equal(await f.control.browse('tracks'), true);
  assert.deepEqual(routes.libraryBrowse, ['GET', '/library/browse']);
  assert.deepEqual(f.calls[1], ['libraryBrowse', { query: { kind: 'tracks', offset: 0, limit: LIMIT } }]);
  assert.equal(f.control.state.target, '');
  assert.equal(f.control.choose(0, 'play'), false);
  assert.equal(f.writes().length, 0);
});

test('new playable Spotify kinds are never fabricated as cached tracks, albums or playlists', async () => {
  for (const kind of ['artist', 'episode', 'show']) for (const category of ['tracks', 'albums', 'playlists']) {
    const f = fixture(async (name, args) => name === 'libraryBrowse'
      ? page(args.query, { total: 1, next_offset: null, items: [item(0, kind)] }) : undefined);
    await open(f, true);
    assert.equal(await f.control.browse(category), false);
    assert.equal(f.control.state.loaded, false);
    assert.equal(f.control.choose(0, 'play'), false);
    assert.deepEqual(f.writes(), []);
  }
});

test('an empty or failed room read does not prevent offline cache browsing', async () => {
  for (const rooms of [[], 'error']) {
    const f = fixture(async (name) => {
      if (name === 'zones') { if (rooms === 'error') throw new Error('Speakers offline'); return rooms; }
    });
    await open(f);
    assert.equal(await f.control.browse('playlists'), true);
    assert.equal(f.control.state.hits.length, LIMIT);
    assert.equal(f.control.choose(0, 'append'), false);
  }
});

test('selecting a group retains a browsed page but invalidates its old consent', async () => {
  const f = fixture(); await open(f); await f.control.browse('albums');
  const first = f.control.state.hits[0];
  assert.equal(f.control.target('Room@S2'), true);
  assert.equal(f.control.state.hits[0], first);
  assert.equal(f.control.choose(0, 'append'), true);
  const pending = f.control.state.pending;
  assert.equal(f.control.target('Room@S1'), true);
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(f.writes().length, 0);
});

test('pagination visits every identity once and requests only bounded pages', async () => {
  const f = fixture(); await open(f); await f.control.browse('tracks');
  const sources = f.control.state.hits.map((hit) => hit.source);
  assert.equal(await f.control.next(), true);
  sources.push(...f.control.state.hits.map((hit) => hit.source));
  assert.equal(await f.control.next(), true);
  sources.push(...f.control.state.hits.map((hit) => hit.source));
  assert.equal(f.control.state.page.offset, 100);
  assert.equal(await f.control.next(), false);
  assert.equal(sources.length, 117);
  assert.equal(new Set(sources).size, 117);
  assert.equal(await f.control.previous(), true);
  assert.equal(f.control.state.page.offset, 50);
  assert.ok(f.calls.filter(([n]) => n === 'libraryBrowse').every(([, args]) => args.query.limit === LIMIT));
});

test('a changed revision clears the page instead of mixing cache versions', async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.offset > 0
    ? page(args.query, { revision: 'cache-new' }) : undefined);
  await open(f, true); await f.control.browse('tracks');
  assert.equal(await f.control.next(), false);
  assert.equal(f.control.state.loaded, false);
  assert.equal(f.control.choose(0, 'play'), false);
  assert.match(f.control.state.message, /changed between pages/);
  assert.equal(f.calls.filter(([n]) => n === 'libraryBrowse').length, 2);
  assert.equal(await f.control.refresh(), true);
  assert.equal(f.control.state.page.offset, 0);
});

test('page turns, filters and category changes consume review consent', async () => {
  for (const change of [(c) => c.next(), (c) => c.query('new'), (c) => c.browse('albums')]) {
    const f = fixture(); await open(f, true); await f.control.browse('tracks');
    f.control.choose(0, 'play'); const pending = f.control.state.pending;
    await change(f.control);
    assert.equal(await f.control.confirm(pending), false);
    assert.equal(f.writes().length, 0);
  }
});

test('opening an album uses its exact identity and never guesses from a repeated title', async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.kind === 'albums'
    ? page(args.query, { total: 2, next_offset: null,
      items: [item(1, 'album', { title: 'Same' }), item(2, 'album', { title: 'Same' })] }) : undefined);
  await open(f); f.control.query('album filter'); await f.control.browse('albums');
  assert.equal(await f.control.openAlbum(1), true);
  assert.deepEqual(f.calls.at(-1), ['libraryBrowse', { query: {
    kind: 'tracks', offset: 0, limit: LIMIT, album_uri: 'spotify:album:Item2' } }]);
  assert.equal(f.control.state.query, '');
  assert.equal(f.control.state.browseAlbum.title, 'Same');
  assert.equal(f.writes().length, 0);
  await f.control.next();
  assert.equal(f.calls.at(-1)[1].query.album_uri, 'spotify:album:Item2');
});

test('late pages cannot resurrect a closed panel or a superseded category', async () => {
  for (const end of [(c) => c.close(), (c) => c.dispose(), (c) => c.browse('playlists')]) {
    const gate = later();
    const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.kind === 'albums' ? gate.promise : undefined);
    await open(f); const old = f.control.browse('albums');
    await end(f.control); gate.resolve(page({ kind: 'albums' }));
    assert.equal(await old, false);
    assert.notEqual(f.control.state.page?.kind, 'albums');
  }
});

test('filters are encoded as data and overlong or invalid parameters make no request', async () => {
  const f = fixture(); await open(f);
  f.control.query('  Björk & strings  '); await f.control.browse('tracks');
  assert.equal(f.calls.at(-1)[1].query.q, 'Björk & strings');
  const before = f.calls.length;
  f.control.query('x'.repeat(MAX_QUERY + 1));
  assert.equal(await f.control.refresh(), false);
  for (const kind of ['artists', '__proto__', '', 'constructor']) assert.equal(await f.control.browse(kind), false);
  assert.equal(await f.control.browse('tracks', -1), false);
  assert.equal(await f.control.browse('tracks', 0, { uri: 'javascript:alert(1)' }), false);
  assert.equal(f.calls.length, before);
});

test('malformed page shapes, counters and identities fail closed', async () => {
  const base = page({ kind: 'tracks' }, { total: 1, next_offset: null, items: [item(0)] });
  const bad = [null, {}, { ...base, kind: 'albums' }, { ...base, offset: 1 },
    { ...base, limit: 100 }, { ...base, total: -1 }, { ...base, revision: '' },
    { ...base, next_offset: 2 }, { ...base, coverage: 'live' },
    { ...base, album_uri: 'spotify:album:Other' }, { ...base, items: [] },
    { ...base, items: [item(0, 'track', { source_uri: 'javascript:alert(1)' })] },
    { ...base, items: [item(0, 'album')] },
    { ...base, total: 2, items: [item(0), item(0)] },
    { ...base, items: [item(0, 'track', { explicit: 'yes' })] }];
  for (const value of bad) {
    const f = fixture(async (name) => name === 'libraryBrowse' ? value : undefined);
    await open(f, true);
    assert.equal(await f.control.browse('tracks'), false);
    assert.equal(f.control.choose(0, 'play'), false);
    assert.equal(f.writes().length, 0);
  }
});

test('album lists preserve explicit and partial-cache metadata', async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse'
    ? page(args.query, { total: 1, next_offset: null, items: [item(0, 'album', { explicit: true, cached_tracks: 1 })] }) : undefined);
  await open(f); assert.equal(await f.control.browse('albums'), true);
  assert.equal(f.control.state.hits[0].explicit, true);
  assert.equal(f.control.state.hits[0].cachedTracks, 1);
  assert.ok(Object.isFrozen(f.control.state.page));
  assert.ok(Object.isFrozen(f.control.state.hits[0]));
});

test('a track from a different album is rejected in a drill-down response', async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse'
    ? page(args.query, { total: 1, next_offset: null, items: [item(0, 'track', { album_uri: 'spotify:album:Other' })] }) : undefined);
  await open(f);
  assert.equal(await f.control.browse('tracks', 0, { uri: 'spotify:album:Album1', title: 'Album' }), false);
  assert.equal(f.control.state.loaded, false);
});

test('browse selections reuse reviewed play, append and next routes exactly once', async () => {
  for (const [kind, action] of [['tracks', 'play'], ['albums', 'append'], ['playlists', 'next']]) {
    const f = fixture(); await open(f, true); await f.control.browse(kind);
    const hit = f.control.state.hits[0];
    assert.equal(f.control.choose(0, action), true);
    assert.equal(f.writes().length, 0);
    assert.equal(await f.control.confirm(), true);
    assert.equal(await f.control.confirm(), false);
    const [route, args] = f.writes()[0];
    assert.equal(route, action === 'play' ? 'libraryPlay' : 'libraryAdd');
    assert.equal(args.body.source_uri, hit.source);
    assert.equal(args.body.zone, 'Room@S2');
    assert.equal(args.body.title, hit.title);
    if (action !== 'play') assert.equal(args.body.next, action === 'next');
  }
});

test('regrouping after a browse review prevents the playback write', async () => {
  let changed = false;
  const f = fixture(async (name) => name === 'zones' && changed
    ? [{ ...zones[1], members: ['Room', 'Different audience'] }] : undefined);
  await open(f, true); await f.control.browse('albums'); f.control.choose(0, 'play'); changed = true;
  assert.equal(await f.control.confirm(), false);
  assert.equal(f.writes().length, 0);
});

test('a write in flight blocks browsing and uncertain replies never replay', async () => {
  const gate = later();
  const f = fixture(async (name) => { if (name === 'libraryPlay') { await gate.promise; throw new Error('Lost reply'); } });
  await open(f, true); await f.control.browse('tracks'); f.control.choose(0, 'play');
  const sent = f.control.confirm();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(await f.control.browse('albums'), false);
  assert.equal(await f.control.next(), false);
  assert.equal(f.control.query('changed'), false);
  assert.equal(await f.control.confirm(), false);
  gate.resolve(); assert.equal(await sent, false);
  assert.equal(f.writes().length, 1);
  assert.match(f.control.state.message, /not repeated/);
  assert.equal(f.control.choose(0, 'play'), false);
});

test('an old page token or result object cannot select a different page item', async () => {
  const f = fixture(); await open(f, true); await f.control.browse('albums');
  const old = f.control.state.page;
  const hit = f.control.state.hits[0];
  await f.control.next();
  assert.equal(await f.control.next(old), false);
  assert.equal(await f.control.openAlbum(0, hit), false);
  assert.equal(f.control.choose(0, 'play', hit), false);
});

test('literal metadata remains text and does not alter identities', async () => {
  const title = '<img src=x onerror="alert(1)"> & "quoted"';
  const f = fixture(async (name, args) => name === 'libraryBrowse'
    ? page(args.query, { total: 1, next_offset: null, items: [item(0, 'track', { title })] }) : undefined);
  await open(f, true); await f.control.browse('tracks'); f.control.choose(0, 'play');
  assert.equal(f.control.state.pending.body.title, title);
  assert.equal(f.control.state.pending.body.source_uri, 'spotify:track:Item0');
});

test('the production route map drives real localhost browse and reviewed queue HTTP', async () => {
  const seen = [];
  const server = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    const url = new URL(req.url, 'http://localhost');
    seen.push({ method: req.method, path: url.pathname, query: Object.fromEntries(url.searchParams), body });
    res.setHeader('Content-Type', 'application/json');
    if (url.pathname === '/zones') res.end(JSON.stringify(zones));
    else if (url.pathname === '/library/browse') res.end(JSON.stringify(page({ kind: url.searchParams.get('kind'),
      offset: Number(url.searchParams.get('offset')) }, { total: 1, next_offset: null, items: [item(0, 'album')] })));
    else if (url.pathname === '/queue') res.end(JSON.stringify({ done: 'Queued', first: 1, count: 3, playing_queue: false }));
    else { res.statusCode = 404; res.end('{}'); }
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const allRoutes = { ...routes, zones: ['GET', '/zones'] };
  try {
    const c = model({ call: async (name, args = {}) => {
      const [method, path] = allRoutes[name];
      const url = new URL(path, `http://127.0.0.1:${server.address().port}`);
      for (const [key, value] of Object.entries(args.query || {})) url.searchParams.set(key, value);
      const response = await fetch(url, { method, headers: { 'Content-Type': 'application/json' },
        body: args.body ? JSON.stringify(args.body) : undefined });
      return response.json();
    } });
    await c.refreshRooms(); c.query('Björk & strings'); await c.browse('albums');
    c.target('Room@S2'); c.choose(0, 'append'); assert.equal(await c.confirm(), true);
    const browse = seen.find((r) => r.path === '/library/browse');
    assert.equal(browse.method, 'GET'); assert.equal(browse.body, '');
    assert.deepEqual(browse.query, { kind: 'albums', offset: '0', limit: '50', q: 'Björk & strings' });
    const writes = seen.filter((r) => r.method === 'POST');
    assert.equal(writes.length, 1);
    assert.deepEqual(JSON.parse(writes[0].body), { zone: 'Room@S2', source_uri: 'spotify:album:Item0', title: 'Title 0', next: false });
  } finally { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); }
});

function completedAlbum(query, revision = 'album-complete') {
  const all = [
    item(0, 'track', { source_uri: 'spotify:track:Repeated', title: 'Prelude', subtitle: '', explicit_known: false }),
    item(1, 'track', { title: 'Scherzo', explicit: true }),
    item(2, 'track', { title: 'Adagio', subtitle: '', explicit_known: false }),
    item(0, 'track', { source_uri: 'spotify:track:Repeated', title: 'Finale', subtitle: '', explicit_known: false, disc_number: 2, track_number: 1 }),
  ].map(row => ({ ...row, album_uri: query.album_uri }));
  const filtered = all.filter(row => !query.q || row.title.toLowerCase().includes(query.q.toLowerCase()));
  return page(query, { total: filtered.length, revision, next_offset: null,
    album_observation: { coverage: 'complete', fetched_at: 100, cached_total: all.length },
    items: filtered.slice(query.offset, query.offset + LIMIT) });
}

test('complete album observations expose expanded tracks, duplicate occurrences and unknown metadata', async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.album_uri ? completedAlbum(args.query) : undefined);
  await open(f); await f.control.browse('albums'); await f.control.openAlbum(0);
  const p = f.control.state.page, hits = f.control.state.hits;
  assert.equal(p.albumObservation.coverage, 'complete');
  assert.equal(p.albumObservation.cached_total, 4);
  assert.equal(p.albumObservation.fetched_at, 100);
  assert.deepEqual(hits.map(hit => [hit.title, hit.disc, hit.track]), [
    ['Prelude', 1, 1], ['Scherzo', 1, 2], ['Adagio', 1, 3], ['Finale', 2, 1],
  ]);
  assert.equal(hits[0].source, hits[3].source);
  assert.equal(hits[0].subtitle, ''); assert.equal(hits[0].explicitKnown, false);
  assert.equal(hits[1].explicitKnown, true); assert.equal(hits[1].explicit, true);
  const description = require('../crates/fsonos-api/src/remote/library.js').albumCoverage(p);
  assert.match(description, /Complete cached album: 4/); assert.match(description, /Read /);
  assert.equal(f.writes().length, 0);
  f.control.query('Finale'); assert.equal(await f.control.refresh(), true);
  assert.equal(f.control.state.page.total, 1);
  assert.equal(f.control.state.page.albumObservation.cached_total, 4);
});

for (const action of ['play', 'append', 'next']) test(`expanded album movement ${action} uses its exact occurrence and URI`, async () => {
  const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.album_uri ? completedAlbum(args.query) : undefined);
  await open(f, true); await f.control.browse('albums'); await f.control.openAlbum(0);
  f.control.query('Finale'); await f.control.refresh();
  assert.equal(f.control.choose(0, action), true);
  assert.equal(f.control.state.pending.hit.disc, 2); assert.equal(f.control.state.pending.hit.track, 1);
  assert.equal(await f.control.confirm(), true);
  const [route, request] = f.writes()[0];
  assert.equal(route, action === 'play' ? 'libraryPlay' : 'libraryAdd');
  assert.equal(request.body.source_uri, 'spotify:track:Repeated');
  assert.equal(request.body.title, 'Finale');
  if (action !== 'play') assert.equal(request.body.next, action === 'next');
  assert.equal(f.writes().length, 1);
});

test('a replaced album observation invalidates its reviewed occurrence', async () => {
  let revision = 'album-before';
  const f = fixture(async (name, args) => name === 'libraryBrowse' && args.query.album_uri ? completedAlbum(args.query, revision) : undefined);
  await open(f, true); await f.control.browse('albums'); await f.control.openAlbum(0);
  f.control.choose(3, 'next'); revision = 'album-after';
  assert.equal(await f.control.confirm(), false);
  assert.match(f.control.state.message, /cached album selection changed/);
  assert.equal(f.writes().length, 0);
});

test('subset coverage remains distinct and malformed complete observations cannot authorize selection', async () => {
  const f = fixture(async (name, args) => {
    if (name !== 'libraryBrowse' || !args.query.album_uri) return;
    const p = completedAlbum(args.query); p.items = [p.items[1]]; p.total = 1;
    p.album_observation = { coverage: 'library_subset', fetched_at: null, cached_total: 1 }; return p;
  });
  await open(f); await f.control.browse('albums'); assert.equal(await f.control.openAlbum(0), true);
  assert.match(require('../crates/fsonos-api/src/remote/library.js').albumCoverage(f.control.state.page), /Library subset: 1/);
  for (const change of [
    p => { p.album_observation.fetched_at = null; },
    p => { p.album_observation.fetched_at = -1; },
    p => { p.album_observation.cached_total = 1; },
    p => { p.items[1].track_number = 1; },
    p => { p.items[0].explicit = true; },
    p => { p.items[0].explicit_known = null; },
  ]) {
    const bad = fixture(async (name, args) => {
      if (name !== 'libraryBrowse' || !args.query.album_uri) return;
      const p = completedAlbum(args.query); change(p); return p;
    });
    await open(bad, true); await bad.control.browse('albums');
    assert.equal(await bad.control.openAlbum(0), false);
    assert.equal(bad.control.choose(0, 'play'), false);
    assert.equal(bad.writes().length, 0);
  }
});
