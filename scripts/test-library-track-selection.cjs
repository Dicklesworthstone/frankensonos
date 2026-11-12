'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const vm = require('node:vm');
const Q = require('../crates/fsonos-api/src/remote/queue-tracks.js');
const R = require('../crates/fsonos-api/src/remote/library.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const Panel = require('../crates/fsonos-api/src/remote/dashboard-library-panel.js');
const id = value => String(value).padStart(22, '0');
const uri = value => `spotify:track:${id(value)}`;
const album = `spotify:album:${id(7)}`, playlist = `spotify:playlist:${id(8)}`;
const copy = value => structuredClone(value);
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve; const promise = new Promise(yes => { resolve = yes; }); return { promise, resolve }; };
const zone = household => ({ household, coordinator_room: 'Room', members: ['Room', 'Study'], transport_state: 'paused' });
const playback = group => ({ zone: copy(group), transport_state: 'paused', volume: 27,
  track: { uri: 'fixture:current-source', queue_position: 1 } });
function catalog(query, changes = {}) {
  const kind = query.kind || 'tracks', offset = Number(query.offset || 0), albumUri = query.album_uri || null;
  const total = albumUri || query.playlist_uri ? 3 : kind === 'tracks' ? 12 : 1;
  const all = Array.from({ length: total }, (_, i) => ({ kind: kind.slice(0, -1),
    source_uri: kind === 'albums' ? album : kind === 'playlists' ? playlist : uri(albumUri || query.playlist_uri ? i === 2 ? 1 : i + 1 : i + 1),
    title: `Movement ${i + 1}`, subtitle: 'Synthetic performer', explicit: i === 1,
    ...(kind === 'albums' ? { cached_tracks: 3 } : {}),
    ...(albumUri ? { album_uri: albumUri, disc_number: i === 2 ? 2 : 1, track_number: i === 2 ? 1 : i + 1 } : {}),
    ...(query.playlist_uri ? { position: [1, 4, 5][i] } : {}) }));
  const end = Math.min(total, offset + 50);
  if (query.playlist_uri) return { playlist_uri: query.playlist_uri, name: 'Repeated playlist', q: query.q || '',
    offset, limit: 50, total, cached_total: total, examined: 5, source_total: 5, coverage: 'complete',
    fetched_at: 100, revision: 'playlist-a', next_offset: end < total ? end : null, items: all.slice(offset, end), ...changes };
  return { kind, offset, limit: 50, total, album_uri: albumUri, coverage: 'cached', revision: 'catalog-a',
    next_offset: end < total ? end : null, items: all.slice(offset, end),
    ...(albumUri ? { album_observation: { coverage: 'complete', fetched_at: 100, cached_total: total } } : {}), ...changes };
}
function data() {
  const zones = [zone('S2'), zone('S1')];
  return { zones, playback: playback(zones[0]), queue: { zone: 'Room', total: 2, revision: 'queue-a', playing: 1,
    items: [{ position: 1, title: 'Current queue entry', uri: 'fixture:queue-entry' }] } };
}
function acknowledgement(body, queue) {
  return { done: 'Queued the ordered selection', zone: 'Room', first: body.next && queue.playing ? queue.playing + 1 : queue.total + 1,
    count: body.tracks.length, playing_queue: queue.playing !== null, notes: [{ detail: 'Current playback was preserved.' }] };
}
async function phone({ collection = 'album', callWrapper } = {}) {
  const value = data(), calls = []; let hook;
  const backend = async (name, args = {}) => {
    calls.push({ name, args: copy(args) });
    const override = hook?.(name, args); if (override !== undefined) return override;
    if (name === 'zones') return copy(value.zones);
    if (name === 'state') return { ...copy(value.playback), zone: copy(value.zones.find(z => `${z.coordinator_room}@${z.household}` === args.params.room)) };
    if (name === 'queue') return copy(value.queue);
    if (name === 'libraryBrowse' || name === 'libraryPlaylist') return catalog(args.query);
    if (name === 'libraryTracks') return acknowledgement(args.body, value.queue);
    throw new Error(`Unexpected phone call ${name}`);
  };
  const control = R.model({ call: callWrapper ? callWrapper(backend) : backend });
  await control.refreshRooms(); control.target('Room@S2');
  if (collection === 'album') await control.browse('tracks', 0, { uri: album, title: 'Repeated album' });
  else if (collection === 'playlist') await control.browse('tracks', 0, null, null, { uri: playlist, title: 'Repeated playlist' });
  else await control.browse('tracks');
  const select = indexes => indexes.forEach(index => assert.equal(control.selectTrack(index, true), true));
  return { control, value, calls, backend, select, hook(fn) { hook = fn; },
    review: action => control.chooseTracks(action), writes: () => calls.filter(call => call.args.body !== undefined) };
}
async function dashboard({ collection = 'album', callWrapper } = {}) {
  const value = data(), calls = []; let hook;
  const backend = async (url, body) => {
    calls.push({ url, body: copy(body) });
    const override = hook?.(url, body); if (override !== undefined) return override;
    if (url === '/zones') return copy(value.zones);
    if (url.endsWith('/state')) return { ...copy(value.playback), zone: copy(value.zones.find(z => url.includes(encodeURIComponent(`${z.coordinator_room}@${z.household}`)))) };
    if (url.endsWith('/dj')) return { zone: 'Room', running: false, steering: { source: 'none' }, next: [] };
    if (url.includes('/queue?')) return copy(value.queue);
    if (url.startsWith('/library/')) return catalog(Object.fromEntries(new URL(url, 'http://localhost').searchParams));
    if (url === '/queue/tracks') return acknowledgement(body, value.queue);
    throw new Error(`Unexpected dashboard call ${url}`);
  };
  const call = callWrapper ? callWrapper(backend) : backend;
  const controller = C.model({ call, now: () => 1000 }), session = S.model({ call, controller, now: () => 1000 });
  const library = L.model({ call }); await controller.refresh(); await session.load(); await library.open();
  if (collection === 'album') await library.album(library.state.page.items[0]);
  else if (collection === 'playlist') { await library.browse('playlists'); await library.playlist(library.state.page.items[0]); }
  else await library.browse('tracks');
  const destination = () => ({ target: controller.state.selected, shape: C.shape(controller.selected().zone) });
  const select = indexes => indexes.forEach(index => assert.equal(library.selectTrack(library.state.page.items[index], true), true));
  return { controller, session, library, value, calls, call, destination, select, hook(fn) { hook = fn; },
    review: action => session.reviewLibraryTracks(library.chooseTracks(), action, destination()),
    writes: () => calls.filter(call => call.body !== undefined) };
}

test('shared batch contract preserves order, episodes and duplicates, normalizes titles, and rejects containers or excess entries', () => {
  const tracks = Q.tracksOf([{ source_uri: uri(1), title: '  First  ' }, { source_uri: `spotify:episode:${id(2)}` },
    { source_uri: uri(1), title: '😀'.repeat(300) }]);
  assert.deepEqual(tracks.map(track => track.source_uri), [uri(1), `spotify:episode:${id(2)}`, uri(1)]);
  assert.equal(tracks[0].title, 'First'); assert.equal([...tracks[2].title].length, 256);
  assert.ok(Object.isFrozen(tracks)); assert.ok(Object.isFrozen(tracks[0]));
  for (const input of [[], Array(11).fill({ source_uri: uri(1) }), [{ source_uri: album }], [{ source_uri: 'spotify:track:short' }]]) {
    assert.throws(() => Q.tracksOf(input));
  }
});

for (const collection of ['album', 'playlist']) for (const action of ['append', 'next']) {
  test(`phone: ${collection} ${action} preserves catalog order and duplicate occurrences in one reviewed write`, async () => {
    const f = await phone({ collection }); f.select([2, 0, 1]);
    assert.equal(await f.review(action), true); const pending = f.control.state.pending;
    assert.equal(f.writes().length, 0); assert.ok(Object.isFrozen(pending.body.tracks));
    assert.deepEqual(pending.body.tracks.map(track => track.source_uri), [uri(1), uri(2), uri(1)]);
    assert.equal(pending.body.expected_revision, 'queue-a'); assert.equal(pending.body.zone, 'Room@S2');
    assert.equal(await f.control.confirm(pending), true); assert.equal(await f.control.confirm(pending), false);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].name, 'libraryTracks');
    assert.equal(f.writes()[0].args.body.next, action === 'next'); assert.deepEqual(f.control.state.selected, []);
    assert.match(f.control.state.message, /playback was preserved/);
  });
  test(`dashboard: ${collection} ${action} uses one native batch with visible exact occurrences`, async () => {
    const f = await dashboard({ collection }); f.select([2, 0, 1]); const review = await f.review(action);
    assert.ok(review); assert.equal(f.writes().length, 0); assert.equal(review.trackSelection.length, 3);
    assert.match(review.trackSelection[2], collection === 'album' ? /disc 2, track 1/ : /playlist position 5/);
    assert.deepEqual(review.body.tracks.map(track => track.source_uri), [uri(1), uri(2), uri(1)]);
    assert.equal(review.body.expected_revision, 'queue-a');
    assert.equal(await f.session.confirm(review), true); assert.equal(await f.session.confirm(review), false);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/queue/tracks');
    assert.equal(f.writes()[0].body.next, action === 'next'); assert.match(f.controller.state.message, /playback was preserved/);
  });
}

for (const kind of ['phone', 'dashboard']) {
  test(`${kind}: page selection is bounded, works offline, and clears on catalog context changes`, async () => {
    const f = await (kind === 'phone' ? phone : dashboard)({ collection: 'tracks' });
    f.select(Array.from({ length: 10 }, (_, i) => 9 - i));
    const model = f.control || f.library;
    assert.equal(kind === 'phone' ? model.selectTrack(10, true) : model.selectTrack(model.state.page.items[10], true), false);
    assert.deepEqual(model.state.selected, Array.from({ length: 10 }, (_, i) => i));
    if (kind === 'phone') model.query('different'); else model.edit('different');
    assert.deepEqual(model.state.selected, []);
    assert.equal(kind === 'phone' ? await model.chooseTracks('next') : model.chooseTracks(), kind === 'phone' ? false : null);
    await model.refresh(); f.select([0]); model.close(); assert.deepEqual(model.state.selected, []);
  });
}

for (const change of ['queue', 'cursor', 'source', 'group', 'catalog', 'occurrence', 'coverage']) {
  test(`phone: changed ${change} stops a reviewed selection before any write`, async () => {
    const f = await phone(); f.select([0, 2]); await f.review('next');
    if (change === 'queue') f.value.queue.revision = 'queue-b';
    if (change === 'cursor') f.value.queue.playing = 2;
    if (change === 'source') f.value.playback.track.uri = 'fixture:another-source';
    if (change === 'group') f.value.zones[0].members = ['Room'];
    if (['catalog', 'occurrence', 'coverage'].includes(change)) f.hook((name, args) => {
      if (name !== 'libraryBrowse') return; const page = catalog(args.query);
      if (change === 'catalog') page.revision = 'catalog-b';
      if (change === 'occurrence') page.items[2].title = 'Different occurrence';
      if (change === 'coverage') page.album_observation.fetched_at++;
      return page;
    });
    assert.equal(await f.control.confirm(), false); assert.equal(f.writes().length, 0);
    assert.match(f.control.state.message, /changed/);
  });
  test(`dashboard: changed ${change} stops a reviewed selection before any write`, async () => {
    const f = await dashboard(); f.select([0, 2]); const review = await f.review('next');
    if (change === 'queue') f.value.queue.revision = 'queue-b';
    if (change === 'cursor') f.value.queue.playing = 2;
    if (change === 'source') f.value.playback.track.uri = 'fixture:another-source';
    if (change === 'group') f.value.zones[0].members = ['Room'];
    if (['catalog', 'occurrence', 'coverage'].includes(change)) f.hook(url => {
      if (!url.startsWith('/library/')) return; const page = catalog(Object.fromEntries(new URL(url, 'http://localhost').searchParams));
      if (change === 'catalog') page.revision = 'catalog-b';
      if (change === 'occurrence') page.items[2].title = 'Different occurrence';
      if (change === 'coverage') page.album_observation.fetched_at++;
      return page;
    });
    assert.equal(await f.session.confirm(review), false); assert.equal(f.writes().length, 0);
    assert.match(f.controller.state.message, /changed/);
  });
}

for (const surface of ['phone', 'dashboard']) test(`${surface}: a next selection while radio plays stays an append without a source change`, async () => {
  const f = await (surface === 'phone' ? phone : dashboard)(); f.select([0, 2]);
  f.value.queue.playing = null; f.value.playback.track = { uri: 'fixture:radio' };
  if (surface === 'dashboard') await f.controller.refresh();
  const review = await f.review('next');
  if (surface === 'phone') assert.equal(await f.control.confirm(), true);
  else { assert.match(review.detail, /added at the end/); assert.equal(await f.session.confirm(review), true); }
  assert.equal(f.writes().length, 1); assert.equal(f.value.playback.track.uri, 'fixture:radio');
});

for (const surface of ['phone', 'dashboard']) for (const mode of ['lost', 'partial_receipt']) {
  test(`${surface}: ${mode} consumes review without retry or single-track fallback`, async () => {
    const f = await (surface === 'phone' ? phone : dashboard)(); f.select([0, 1, 2]); const review = await f.review('append');
    f.hook((name, value) => {
      const body = surface === 'phone' ? value.body : value;
      if (!body) return;
      if (mode === 'lost') return Promise.reject(new Error('Injected lost acknowledgement'));
      return { ...acknowledgement(body, f.value.queue), count: 1 };
    });
    if (surface === 'phone') {
      assert.equal(await f.control.confirm(), false); assert.equal(await f.control.confirm(), false);
      assert.match(f.control.state.message, /not repeated/);
    } else {
      assert.equal(await f.session.confirm(review), false); assert.equal(f.controller.state.uncertain, true);
      assert.equal(await f.session.confirm(review), false);
    }
    assert.equal(f.writes().length, 1);
  });
}

test('a phone group change during destination inspection cannot publish a review for the old group', async () => {
  const f = await phone(), gate = deferred(); f.select([0, 2]);
  f.hook(name => name === 'queue' ? gate.promise : undefined);
  const run = f.review('next'); await tick(); f.control.target('Room@S1'); gate.resolve(copy(f.value.queue));
  assert.equal(await run, false); assert.equal(f.control.state.pending, null); assert.equal(f.writes().length, 0);
  assert.deepEqual(f.control.state.selected, [0, 2]);
});

test('dashboard queue events and hiding during asynchronous review consume old selection consent', async () => {
  const f = await dashboard(); f.select([0, 2]); const review = await f.review('next');
  f.session.queueChanged('Room'); assert.equal(await f.session.confirm(review), false);
  const gate = deferred(); f.hook(url => url.includes('/queue?') ? gate.promise : undefined);
  const run = f.review('next'); await tick(); f.session.suspend(); gate.resolve(copy(f.value.queue));
  assert.equal(await run, null); assert.equal(f.session.state.pending, null); assert.equal(f.writes().length, 0);
});

test('dashboard rechecks the playback source after a delayed catalog read, before committing the batch', async () => {
  const f = await dashboard(); f.select([0, 2]); const review = await f.review('next'), gate = deferred();
  let query;
  f.hook(url => { if (url.startsWith('/library/')) { query = Object.fromEntries(new URL(url, 'http://localhost').searchParams); return gate.promise; } });
  const run = f.session.confirm(review); await tick(); assert.ok(query);
  f.value.playback.track.uri = 'fixture:new-intent'; gate.resolve(catalog(query));
  assert.equal(await run, false); assert.equal(f.writes().length, 0); assert.match(f.controller.state.message, /Playback changed/);
});

test('one reviewed batch crosses a real HTTP connection; a lost POST response is never replayed', async t => {
  let backend, posts = 0;
  const server = http.createServer(async (req, res) => {
    let text = ''; for await (const chunk of req) text += chunk;
    const result = await backend(req.url, text ? JSON.parse(text) : undefined);
    if (req.method === 'POST') { posts++; req.socket.destroy(); return; }
    res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(result));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await dashboard({ callWrapper: raw => {
    backend = raw; return C.request({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) });
  } });
  f.select([2, 0]); const review = await f.review('next');
  assert.equal(await f.session.confirm(review), false); assert.equal(posts, 1); assert.equal(f.controller.state.uncertain, true);
  assert.deepEqual(f.writes()[0].body.tracks.map(track => track.source_uri), [uri(1), uri(1)]);
  await f.controller.refresh(); assert.equal(await f.review('next'), null); assert.equal(posts, 1);
});

test('both served assets include the batch contract before library consumers and use the queue write budget', async () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const phoneAsset = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  const desktopAsset = fs.readFileSync(path.join(root, 'http/remote/dashboard.rs'), 'utf8');
  assert.match(phoneAsset, /\{QUEUE_TRACKS_SCRIPT\}\\n\{PLAYLIST_SCRIPT\}\\n\{LIBRARY_SCRIPT\}/);
  assert.match(desktopAsset, /\{QUEUE_TRACKS\}\\n\{PLAYLIST\}\\n\{LIBRARY\}\\n\{SESSION\}/);
  assert.deepEqual(R.routes.libraryTracks, ['POST', '/queue/tracks']);
  const budgets = [], call = C.request({ fetcher: async () => new Response('{}'), schedule(fn, ms) { budgets.push(ms); return 1; }, cancel() {} });
  await call('/queue/tracks', { zone: 'Room@S2', tracks: [{ source_uri: uri(1) }] });
  assert.deepEqual(budgets, [600000]);
});

function documentFixture() {
  const roots = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { hidden: false, activeElement: null, createTextNode: text => String(text),
    getElementById: id => roots.get(id) || null, addEventListener() {}, removeEventListener() {},
    createElement(tag) {
      const listeners = new Map();
      const node = { tagName: tag.toUpperCase(), children: [], attributes: {}, dataset: {}, parent: null,
        value: '', disabled: false, checked: false, hidden: false, open: false, isConnected: true, scrollTop: 0, _text: '',
        get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
        set textContent(value) { this._text = String(value); this.replaceChildren(); },
        setAttribute(name, value) {
          this.attributes[name] = String(value);
          if (['type', 'value', 'id'].includes(name)) this[name] = String(value);
          if (['disabled', 'checked', 'hidden'].includes(name)) this[name] = true;
          if (name.startsWith('data-')) this.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = String(value);
        },
        getAttribute(name) {
          if (name.startsWith('data-')) return this.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] ?? null;
          return this.attributes[name] ?? null;
        },
        append(...children) { for (const child of children) { if (child == null) continue; if (typeof child === 'object') child.parent = this; this.children.push(child); } },
        replaceChildren(...children) { for (const child of this.children) if (typeof child === 'object') child.parent = null; this.children = []; this.append(...children); },
        before(child) { if (!this.parent) return; child.parent = this.parent; this.parent.children.splice(this.parent.children.indexOf(this), 0, child); },
        remove() { if (this.parent) this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; this.isConnected = false; },
        contains(child) { return child === this || descendants(this).includes(child); },
        closest() { for (let current = this; current; current = current.parent) if (current.hidden || current.inert) return current; return null; },
        querySelectorAll(selector) { return descendants(this).filter(child => selector.startsWith('[')
          ? child.getAttribute(selector.slice(1, -1)) !== null : child.tagName === selector.toUpperCase()); },
        querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
        addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
        removeEventListener(name, listener) { listeners.set(name, (listeners.get(name) || []).filter(value => value !== listener)); },
        fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; },
      };
      return node;
    } };
  const dialog = doc.createElement('dialog'), results = doc.createElement('div'); dialog.open = true; dialog.append(results);
  roots.set('search-dialog', dialog); roots.set('search-results', results);
  return doc;
}
const findButton = (node, label) => node.querySelectorAll('button').find(button => button.textContent === label);
const settled = async condition => { for (let i = 0; i < 30 && !condition(); i++) await tick(); assert.ok(condition(), 'the component reached its expected state'); };

test('phone checkbox controls open an exact ordered review and submit once to the chosen household', async t => {
  const f = await phone(), doc = documentFixture(), previous = global.document; global.document = doc;
  let panel;
  t.after(() => { panel?.dispose(); global.document = previous; });
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const el = vm.runInNewContext(source.slice(source.indexOf('function el('), source.indexOf('\nlet toastTimer')) + '\nel;', { document: doc });
  panel = R.panel({ call: f.backend, el });
  panel.node.open = true; panel.node.fire('toggle'); await settled(() => panel.node.querySelectorAll('option').length === 3);
  const group = panel.node.querySelector('select'); group.value = 'Room@S1'; group.fire('change');
  findButton(panel.node, 'Browse albums').fire('click'); await settled(() => !!findButton(panel.node, 'View cached tracks'));
  findButton(panel.node, 'View cached tracks').fire('click');
  await settled(() => panel.node.querySelectorAll('input').filter(input => input.type === 'checkbox').length === 3);
  for (const index of [2, 0]) {
    const checkbox = panel.node.querySelectorAll('input').filter(input => input.type === 'checkbox')[index];
    checkbox.checked = true; checkbox.fire('change');
  }
  assert.match(panel.node.textContent, /2 selected/); findButton(panel.node, 'Queue selected next').fire('click');
  await settled(() => !!findButton(panel.node, 'Confirm queue selected next'));
  assert.match(panel.node.textContent, /Destination: Room \+ Study \(S1\)/);
  const list = panel.node.querySelector('ol'); assert.equal(list.children.length, 2);
  assert.match(list.children[0].textContent, /Movement 1.*Disc 1, track 1/);
  assert.match(list.children[1].textContent, /Movement 3.*Disc 2, track 1/);
  const confirm = findButton(panel.node, 'Confirm queue selected next'); confirm.fire('click'); confirm.fire('click');
  await settled(() => f.writes().length === 1 && panel.node.textContent.includes('Current playback was preserved.'));
  assert.equal(f.writes()[0].args.body.zone, 'Room@S1'); assert.deepEqual(f.writes()[0].args.body.tracks.map(track => track.source_uri), [uri(1), uri(1)]);
});

test('dashboard checkbox controls hand the ordered selection to its shared review and command lane', async t => {
  const f = await dashboard(), doc = documentFixture();
  const panel = Panel.create({ call: f.call, controller: f.controller, session: f.session, doc });
  t.after(() => panel.dispose()); panel.node.open = true; panel.node.fire('toggle');
  await settled(() => !!findButton(panel.node, 'View cached tracks')); findButton(panel.node, 'View cached tracks').fire('click');
  await settled(() => panel.node.querySelectorAll('input').filter(input => input.type === 'checkbox').length === 3);
  for (const index of [2, 0]) {
    const checkbox = panel.node.querySelectorAll('input').filter(input => input.type === 'checkbox')[index];
    checkbox.checked = true; checkbox.fire('change');
  }
  assert.match(panel.node.textContent, /2 selected/); findButton(panel.node, 'Add selected to end').fire('click');
  await settled(() => !!f.session.state.pending); const review = f.session.state.pending;
  assert.equal(doc.getElementById('search-dialog').open, false); assert.deepEqual(panel.model.state.selected, []);
  assert.deepEqual(review.body.tracks.map(track => track.source_uri), [uri(1), uri(1)]);
  assert.equal(review.trackSelection.length, 2); assert.match(review.trackSelection[1], /disc 2, track 1/);
  assert.equal(await f.session.confirm(review), true); assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].body.next, false);
});

for (const surface of ['phone', 'dashboard']) test(`${surface}: cached tracks can be selected with no discovered speakers, while review requires a destination`, async () => {
  const wrapper = backend => (name, args) => name === (surface === 'phone' ? 'zones' : '/zones') ? Promise.resolve([]) : backend(name, args);
  const f = await (surface === 'phone' ? phone : dashboard)({ callWrapper: wrapper }); f.select([0, 2]);
  assert.deepEqual((f.control || f.library).state.selected, [0, 2]);
  if (surface === 'phone') assert.equal(await f.control.chooseTracks('next'), false);
  else assert.equal(await f.session.reviewLibraryTracks(f.library.chooseTracks(), 'next', null), null);
  assert.equal(f.writes().length, 0);
});
