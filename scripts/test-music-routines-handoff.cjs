'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const R = require('../crates/fsonos-api/src/remote/favorite-routines.js');
const L = require('../crates/fsonos-api/src/remote/library.js');
const ID = '0123456789ABCDEFabcdef';
const clone = value => structuredClone(value);
const zones = () => [
  { household: 'S1', coordinator_room: 'Den', members: ['Den', 'Hall'] },
  { household: 'S2', coordinator_room: 'Den', members: ['Den'] },
];
const destination = household => ({ target: `Den@${household}`, household,
  members: household === 'S1' ? ['Den@S1', 'Hall@S1'] : ['Den@S2'] });
const music = (kind = 'album', household = null) => L.routineSourceOf({ kind, source: `spotify:${kind}:${ID}`,
  title: `Fixture ${kind}`, subtitle: 'Fixture artist', favorite: null }, household ? destination(household) : null);
const favorite = (household = 'S1') => L.routineSourceOf({ kind: 'favorite', source: null,
  favorite: 'FV:2/1', title: 'Morning music', subtitle: 'Fixture playlist' }, destination(household));
const draft = patch => ({ label: 'Morning listening', cadence: 'weekdays', time: '07:30',
  minutes: '45', volume: '', unmute: false, ...patch });
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };

function fixture() {
  const f = { zones: zones(), calls: [], scenes: [], schedules: [], stored: null, hook: null,
    favorites: [{ id: 'FV:2/1', title: 'Morning music', kind: 'container', description: 'Fixture playlist' }] };
  f.call = async (name, args = {}) => {
    f.calls.push([name, clone(args)]);
    if (f.hook) { const value = await f.hook(name, args); if (value !== undefined) return value; }
    if (['zones', 'routineZones'].includes(name)) return clone(f.zones);
    if (name === 'routineFavorites') return clone(f.favorites);
    if (name === 'routineScenes') return clone(f.scenes);
    if (name === 'routineSchedules') return clone(f.schedules);
    if (name === 'routineSaveScene') {
      const saved = { name: args.params.name, saved: 1900000000, ...clone(args.body) };
      if (saved.groups[0].source.kind === 'favorite') saved.groups[0].source = {
        kind: 'favorite', name: f.favorites.find(item => item.id === saved.groups[0].source.name).title,
        uri: 'x-file:fixture-music',
      };
      f.scenes.push(saved); return clone(saved);
    }
    if (name === 'routineAdd') {
      const saved = { id: 1, when: args.body.when, action: 'apply_scene', scene: args.body.scene,
        enabled: true, creator: 'fixture', summary: 'Fixture routine' };
      f.schedules.push(saved); return clone(saved);
    }
    throw new Error(`Unexpected route ${name}`);
  };
  f.journal = { load: () => clone(f.stored), save: value => { f.stored = clone(value); }, clear: () => { f.stored = null; } };
  f.control = R.model({ call: f.call, journal: f.journal, nonce: () => 'a'.repeat(32) });
  f.writes = () => f.calls.filter(([name]) => ['routineSaveScene', 'routineAdd'].includes(name));
  return f;
}

for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) test(`library ${kind} handoff fills a routine and uses only reviewed scene/schedule writes`, async () => {
  const f = fixture(), selection = music(kind, 'S1');
  f.control.resumeDraft();
  const patch = await f.control.importSource(selection);
  assert.equal(patch.source_uri, `spotify:${kind}:${ID}`);
  assert.equal(patch.selection.title, `Fixture ${kind}`);
  assert.equal(f.control.state.group.target, 'Den@S1');
  assert.equal(f.control.state.pending, null);
  assert.deepEqual(f.writes(), []);
  assert.equal(f.calls.filter(([name]) => name === 'routineFavorites').length, 0);
  f.control.review(draft(patch));
  assert.deepEqual(f.writes(), []);
  await f.control.confirm(f.control.state.pending);
  assert.equal(f.control.state.phase, 'complete', f.control.state.message);
  assert.deepEqual(f.writes().map(([name]) => name), ['routineSaveScene', 'routineAdd']);
  assert.deepEqual(f.scenes[0].groups[0].source, { kind: 'uri', uri: `spotify:${kind}:${ID}` });
});

for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) test(`${kind} handoff without a destination never invents one`, async () => {
  const f = fixture(); f.control.resumeDraft();
  const patch = await f.control.importSource(music(kind));
  assert.equal(patch.source_kind, 'spotify');
  assert.equal(f.control.state.group, null);
  assert.equal(f.control.state.groups.length, 2);
  f.control.review(draft(patch));
  assert.equal(f.control.state.pending, null);
  assert.deepEqual(f.writes(), []);
});

test('favorite handoff resolves exact metadata in its original household before filling the source', async () => {
  const f = fixture(); f.control.resumeDraft();
  const patch = await f.control.importSource(favorite());
  assert.equal(patch.favorite, 'FV:2/1');
  assert.equal(f.control.state.group.target, 'Den@S1');
  assert.deepEqual(f.calls.find(([name]) => name === 'routineFavorites'), ['routineFavorites', { query: { zone: 'Den@S1' } }]);
  f.control.review(draft(patch));
  await f.control.confirm(f.control.state.pending);
  assert.equal(f.control.state.phase, 'complete');
  assert.deepEqual(f.writes()[0][1].body.groups[0].source, { kind: 'favorite', name: 'FV:2/1' });
});

test('same favorite ID in another household is never selected by the handoff', () => {
  const selection = R.librarySourceOf(favorite());
  const favorites = R.favoritesOf([{ id: 'FV:2/1', title: selection.title, kind: 'track', description: selection.subtitle }]);
  assert.equal(R.libraryFavorite(selection, favorites, R.groupsOf(zones())[1]), null);
  assert.equal(R.libraryFavorite(selection, favorites, R.groupsOf(zones())[0]).id, 'FV:2/1');
});

test('changed or ambiguous favorites leave the favorite field empty with a useful message', async () => {
  for (const change of ['title', 'description', 'missing', 'duplicate title', 'unplayable']) {
    const f = fixture(); f.control.resumeDraft();
    if (change === 'title') f.favorites[0].title = 'Another collection';
    if (change === 'description') f.favorites[0].description = 'Another source';
    if (change === 'missing') f.favorites = [];
    if (change === 'duplicate title') f.favorites.push({ ...f.favorites[0], id: 'FV:2/2' });
    if (change === 'unplayable') f.favorites[0].kind = 'unplayable';
    const patch = await f.control.importSource(favorite());
    assert.equal(patch.favorite, '', change);
    assert.match(f.control.state.message, /favorite changed|ambiguous/);
    assert.deepEqual(f.writes(), []);
  }
});

test('a changed group preserves the music but requires a new destination choice', async () => {
  for (const selection of [music('album', 'S1'), favorite()]) {
    const f = fixture(); f.zones[0].members.push('New room'); f.control.resumeDraft();
    const patch = await f.control.importSource(selection);
    assert.equal(f.control.state.group, null);
    assert.equal(patch.source_kind, selection.kind);
    assert.equal(patch.favorite, '');
    assert.match(f.control.state.message, /group.*changed/);
    assert.deepEqual(f.writes(), []);
  }
});

test('using different Spotify music preserves the existing draft group when requested', async () => {
  const f = fixture(); await f.control.refresh(); await f.control.choose('Den@S1', 'spotify');
  const patch = await f.control.importSource(music('playlist', 'S2'), { keepGroup: true });
  assert.equal(patch.source_kind, 'spotify');
  assert.equal(f.control.state.group.target, 'Den@S1');
  f.zones[0].members.push('New room');
  await f.control.importSource(music('track', 'S2'), { keepGroup: true });
  assert.equal(f.control.state.group, null, 'a changed draft group is not replaced by the library group');
  assert.deepEqual(f.writes(), []);
});

test('a failed or superseded import retains the prior group and source choices', async () => {
  for (const action of ['failure', 'superseded', 'close', 'dispose']) {
    const f = fixture(); await f.control.refresh(); await f.control.choose('Den@S1');
    const originalGroup = f.control.state.group, originalFavorites = f.control.state.favorites;
    const wait = deferred(); let current = true;
    f.hook = async (name) => {
      if (name !== 'routineZones') return undefined;
      if (action === 'failure') throw new Error('Fixture connection failed');
      return wait.promise;
    };
    const work = f.control.importSource(music('album', 'S2'), { current: () => current });
    if (action === 'superseded') current = false;
    if (action === 'close') f.control.close();
    if (action === 'dispose') f.control.dispose();
    wait.resolve(zones());
    assert.equal(await work, null, action);
    assert.equal(f.control.state.group, originalGroup, action);
    assert.equal(f.control.state.favorites, originalFavorites, action);
    assert.deepEqual(f.writes(), []);
  }
});

test('an active review or interrupted setup cannot be replaced by an imported source', async () => {
  const f = fixture(); f.control.resumeDraft();
  const patch = await f.control.importSource(favorite());
  f.control.review(draft(patch));
  const reviewed = f.control.state.pending, reads = f.calls.length;
  assert.equal(await f.control.importSource(music()), null);
  assert.equal(f.control.state.pending, reviewed);
  assert.equal(f.calls.length, reads);
  f.hook = async (name) => { if (name === 'routineSaveScene') throw new Error('Fixture lost scene response'); };
  await f.control.confirm(reviewed);
  assert.equal(f.control.state.phase, 'unknown');
  const attempt = clone(f.control.state.attempt), journal = clone(f.stored), calls = f.calls.length;
  assert.equal(await f.control.importSource(music()), null);
  assert.deepEqual(f.control.state.attempt, attempt);
  assert.deepEqual(f.stored, journal);
  assert.equal(f.calls.length, calls);
});

test('reopening an unsent draft preserves its group and favorites without a new request', async () => {
  const f = fixture(); await f.control.refresh(); await f.control.choose('Den@S1');
  const group = f.control.state.group, favorites = f.control.state.favorites, calls = f.calls.length;
  f.control.close(); f.control.resumeDraft();
  assert.equal(f.control.state.group, group);
  assert.equal(f.control.state.favorites, favorites);
  assert.equal(f.calls.length, calls);
});

test('handoff payloads are bounded music data and reject timing, writes, ambiguous sources and incomplete identity', () => {
  const base = music('album', 'S1');
  for (const payload of [{ ...base, when: 'daily 06:00' }, { ...base, action: 'apply_scene' },
    { ...base, volume: 100 }, { ...base, source_uri: `spotify:genre:${ID}` },
    { ...base, source_uri: 'spotify:track:short' }, { ...base, favorite: 'FV:2/1' },
    { ...base, title: '' }, { ...base, subtitle: '\u0000' },
    { ...base, group: { ...base.group, members: ['Den@S2'] } },
    { ...favorite(), group: null }, { ...favorite(), source_uri: `spotify:track:${ID}` }]) {
    assert.throws(() => R.librarySourceOf(payload));
  }
  const frozen = R.librarySourceOf(base);
  assert.ok(Object.isFrozen(frozen.group.members));
  assert.equal(L.routineSourceOf({ source: 'spotify:track:short', title: 'Incomplete', subtitle: '' }), null);
});

test('library only hands off the currently observed result and does not need a destination for cached music', async () => {
  const raw = { kind: 'album', source_uri: `spotify:album:${ID}`, title: 'Fixture album', subtitle: 'Fixture artist', explicit: false, cached_tracks: 1 };
  const calls = [];
  const library = L.model({ call: async (name, args) => {
    calls.push(name);
    return name === 'zones' ? zones() : {
      kind: args.query.kind, offset: 0, limit: 50, album_uri: null, coverage: 'cached', total: 1,
      next_offset: null, revision: 'fixture', items: [raw],
    };
  } });
  await library.refreshRooms(); await library.browse('albums');
  const observed = library.state.hits[0], selection = library.routine(0, observed);
  assert.equal(selection.group, null);
  assert.equal(selection.source_uri, raw.source_uri);
  assert.equal(library.state.pending, null);
  library.query('Something else');
  assert.equal(library.routine(0, observed), null);
  assert.deepEqual(calls, ['zones', 'libraryBrowse']);
});

test('discovery to imported routine crosses a real HTTP fixture with no writes before confirmation', async (t) => {
  const http = require('node:http');
  const { once } = require('node:events');
  const f = fixture(), requests = [];
  const raw = { kind: 'album', source_uri: `spotify:album:${ID}`, title: 'Fixture album', subtitle: 'Fixture artist' };
  const server = http.createServer(async (req, res) => {
    let input = ''; for await (const chunk of req) input += chunk;
    const url = new URL(req.url, 'http://fixture.invalid');
    requests.push([req.method, req.url]);
    let result;
    if (url.pathname === '/zones') result = zones();
    else if (url.pathname === '/library/search') result = [raw];
    else if (url.pathname === '/scenes') result = clone(f.scenes);
    else if (url.pathname === '/schedules') result = req.method === 'GET' ? clone(f.schedules)
      : await f.call('routineAdd', { body: JSON.parse(input) });
    else if (url.pathname.startsWith('/scenes/') && req.method === 'PUT') result = await f.call('routineSaveScene', {
      params: { name: decodeURIComponent(url.pathname.slice(8)) }, body: JSON.parse(input),
    });
    else throw new Error(`Unexpected HTTP request ${req.method} ${req.url}`);
    res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(result));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));
  const routes = { zones: ['GET', '/zones'], ...L.routes, ...R.routes };
  const call = async (name, { params = {}, query = {}, body } = {}) => {
    const [method, route] = routes[name];
    const url = new URL(route.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key])), `http://127.0.0.1:${server.address().port}`);
    for (const [key, value] of Object.entries(query)) url.searchParams.set(key, value);
    const reply = await fetch(url, { method, headers: { 'content-type': 'application/json' }, ...(body ? { body: JSON.stringify(body) } : {}) });
    assert.equal(reply.status, 200); return reply.json();
  };
  const library = L.model({ call }); await library.refreshRooms(); library.target('Den@S1'); library.query('Fixture'); await library.search();
  const routine = R.model({ call, journal: f.journal, nonce: () => 'b'.repeat(32) }); routine.resumeDraft();
  const patch = await routine.importSource(library.routine(0));
  assert.equal(patch.source_uri, raw.source_uri);
  assert.ok(requests.every(([method]) => method === 'GET'));
  routine.review(draft(patch)); await routine.confirm(routine.state.pending);
  assert.equal(routine.state.phase, 'complete', routine.state.message);
  assert.deepEqual(requests.filter(([method]) => method !== 'GET').map(([method]) => method), ['PUT', 'POST']);
  assert.equal(f.scenes.length, 1); assert.equal(f.schedules.length, 1);
});
