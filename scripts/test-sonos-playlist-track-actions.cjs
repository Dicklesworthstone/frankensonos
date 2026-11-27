// Exercise the production phone and dashboard command models against a native
// catalog, including changes between inspection, review and dispatch.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Targets = require('../crates/fsonos-api/src/remote/source-targets.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(predicate) {
  for (let i = 0; i < 200 && !predicate(); ++i) await Promise.resolve();
  assert.ok(predicate(), 'reached the asynchronous command boundary');
}

function documentFixture() {
  const nodes = new Map(), doc = { activeElement: null, hidden: false, addEventListener() {}, removeEventListener() {},
    createElement(tag) {
      const listeners = new Map();
      return { tag, ownerDocument: doc, children: [], hidden: false, disabled: false, open: false, textContent: '', value: '', attributes: {},
        append(...values) { this.children.push(...values); }, replaceChildren(...values) { this.children = values; },
        setAttribute(key, value) { this.attributes[key] = value; }, getAttribute(key) { return this.attributes[key] ?? null; },
        addEventListener(key, listener) { listeners.set(key, listener); }, removeEventListener(key) { listeners.delete(key); },
        fire(key, event = {}) { return listeners.get(key)?.({ target: this, preventDefault() {}, ...event }); },
        focus() { doc.activeElement = this; }, scrollIntoView() {},
        contains(node) { return this === node || this.children.some(child => child?.contains?.(node)); },
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      };
    }, getElementById(id) { if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id }); return nodes.get(id); },
  };
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key === 'class') node.className = value;
      else if (key.startsWith('aria-')) node.setAttribute(key, value);
      else node[key] = value;
    }
    node.append(...children); return node;
  };
  return { doc, el };
}
function descendants(node) { return [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? descendants(child) : [])]; }

async function fixture(kind, { dom = false, destinations = false } = {}) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' }));
  if (destinations) zones.push({ household: 'S1', coordinator_room: 'Office', members: ['Office', 'Side'], transport_state: 'paused' });
  const player = (home, room) => 'fixture-' + home + '-' + room;
  const targetZones = target => target === 'everywhere' ? zones
    : zones.filter(zone => target === 'downstairs' || target === 'all@S1'
      ? zone.household === 'S1' : target === zone.coordinator_room + '@' + zone.household);
  const resolution = target => ({ target, revision: 'a'.repeat(64), matched: 'alias',
    rooms: targetZones(target).map(zone => ({ household: zone.household, room: zone.coordinator_room,
      player: player(zone.household, zone.coordinator_room), coordinator: player(zone.household, zone.coordinator_room) })),
    groups: targetZones(target).map(zone => ({ household: zone.household, coordinator_room: zone.coordinator_room,
      coordinator: player(zone.household, zone.coordinator_room), members: zone.members.map(room => ({
        household: zone.household, room, player: player(zone.household, room), coordinator: player(zone.household, zone.coordinator_room),
      })) })) });
  const catalog = [{ id: 'SQ:7', title: 'Saved & repeated' }, { id: 'SQ:8', title: 'Other list' }];
  const entries = [
    { title: 'First occurrence', uri: 'x-file-cifs://example.invalid/repeated.flac', creator: 'Artist' },
    { title: 'Second occurrence', uri: 'x-file-cifs://example.invalid/repeated.flac', creator: 'Artist' },
    { title: 'Unavailable' },
  ];
  const rows = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, { zone, transport_state: 'paused', volume: 12, mute: false,
    track: { uri: 'fixture:playing', title: 'Owner song', queue_position: 1, position_secs: 10, duration_secs: 300 } }]));
  const queues = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, { zone: zone.coordinator_room, total: 2, revision: 'queue:' + zone.household + (zone.coordinator_room === 'Den' ? '' : ':' + zone.coordinator_room),
    playing: 1, items: [{ position: 1, uri: 'fixture:playing', title: 'Owner song' }] }]));
  let revision = 'native:1', reader = null, writer = null, api, controller;
  const calls = [], lanes = [];
  const read = url => {
    const parsed = new URL(url, 'http://localhost');
    const query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/rooms') return zones.map(zone => ({ name: zone.coordinator_room,
      household: zone.household, zone: zone.coordinator_room, aliases: zone.household === 'S1' ? ['downstairs'] : [] }));
    if (parsed.pathname === '/rooms/resolve') return copy(resolution(query.target));
    if (parsed.pathname === '/schedules') return [];
    if (parsed.pathname === '/sonos/playlists') return copy(catalog);
    if (parsed.pathname === '/sonos/playlists/contents') {
      if (query.expected_revision && query.expected_revision !== revision) throw new Error('The saved playlist changed. Read tracks again.');
      const offset = Number(query.offset), limit = Number(query.limit);
      const items = entries.slice(offset, offset + limit).map((item, index) => ({ ...item, position: offset + index + 1 }));
      return { zone: 'Den', playlist: copy(catalog.find(item => item.id === query.playlist)), revision, total: entries.length,
        offset, next_offset: offset + items.length < entries.length ? offset + items.length : null, items };
    }
    const target = decodeURIComponent(parsed.pathname.split('/')[2] || '');
    if (parsed.pathname.endsWith('/state')) return copy(rows.get(target));
    if (parsed.pathname.endsWith('/queue')) return copy(queues.get(target));
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.ok(['/sonos/playlists/track', '/sonos/playlists/tracks'].includes(url), 'never fall back to a whole playlist or raw URI');
    const positions = url.endsWith('/tracks') ? body.positions : [body.position];
    const q = queues.get(body.zone);
    if (body.target) {
      const groups = targetZones(body.target), single = groups.length === 1 ? groups[0] : null;
      const summary = { done: 'Selected tracks delivered', changed: true, notes: [],
        zone: single ? single.coordinator_room : body.target, playlist: body.playlist, mode: body.mode,
        positions: [...positions], titles: positions.map(position => entries[position - 1].title) };
      if (body.mode === 'play') return { ...summary, ...(single ? {} : { batch: { complete: true, results: groups.map(zone => ({
        household: zone.household, target: player(zone.household, zone.coordinator_room), zone: zone.coordinator_room,
        result: { status: 'succeeded', done: 'Selected tracks playing', changed: true, notes: [] },
      })) } }) };
      return { ...summary, queue_batch: { done: 'Added to selected groups', complete: true, logged: true, undoable: true, failed: [],
        added: groups.map(zone => { const queue = queues.get(zone.coordinator_room + '@' + zone.household); return {
          household: zone.household, coordinator: player(zone.household, zone.coordinator_room), queue: {
            zone: zone.coordinator_room, done: 'Selected tracks queued', first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1,
            count: positions.length, playing_queue: queue.playing !== null,
          },
        }; }) } };
    }
    return { done: body.mode === 'play' ? 'Selected track playing' : 'One selected track queued', changed: true, notes: [{ detail: 'Policy applied.' }],
      zone: 'Den', playlist: body.playlist, mode: body.mode,
      ...(url.endsWith('/tracks') ? { positions: [...positions], titles: positions.map(position => entries[position - 1].title) }
        : { position: body.position, title: entries[body.position - 1].title }),
      ...(body.mode === 'play' ? {} : { first: body.mode === 'next' ? q.playing + 1 : q.total + 1, count: positions.length, playing_queue: true }) };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  const view = dom ? documentFixture() : null;
  if (dom && kind === 'dashboard') view.doc.getElementById('source-contents').append(view.doc.getElementById('source-contents-items'));
  if (kind === 'phone') {
    const options = { executeSet(groups, work) { lanes.push(groups); return work(); }, call(name, args = {}) {
      const [method, path] = { ...Phone.routes, ...Targets.routes }[name] || (name === 'zones' ? ['GET', '/zones'] : ['GET', '/zones/{room}/state']);
      const url = path.replace('{room}', encodeURIComponent(args.params?.room)) + (args.query ? '?' + new URLSearchParams(args.query) : '');
      return call(url, method === 'POST' ? args.body : undefined);
    } };
    api = dom ? Phone.panel({ ...options, el: view.el, document: view.doc }) : Phone.model(options);
    await api.refresh(); api.edit('target', 'Den@S1');
  } else {
    controller = Core.model({ call, changed: () => api?.observe() });
    api = dom ? Dashboard.panel({ call, controller, document: view.doc }) : Dashboard.model({ call, controller });
    assert.equal(await controller.refresh(), true); controller.connection(true); api.open();
  }
  api.edit('sourceType', 'playlist'); await api.loadPlaylists(); api.edit('playlist', 'SQ:7');
  assert.equal(await api.loadContents(), true);
  return { api, controller, calls, entries, rows, queues, catalog, zones, resolution, lanes, doc: view?.doc,
    writes: () => calls.filter(call => call.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, revision(value) { revision = value; },
    async review(position = 2, mode = 'play', page) { await api.reviewTrack(position, mode, page); return api.state.pending; },
    confirm(pending = api.state.pending) { return kind === 'phone' ? api.confirm(pending) : api.apply(pending); },
    selectOtherHousehold() { if (kind === 'phone') api.edit('target', 'Den@S2'); else { controller.select('Den@S2'); api.observe(); } },
  };
}

for (const kind of ['phone', 'dashboard']) {
  test(kind + ': native playlist changes retire reviewed occurrences only in their own household', async () => {
    const f = await fixture(kind), reviewed = await f.review(), page = f.api.state.contents;
    f.api.catalogChanged({ household: 'S2', favorites: false, playlists: true });
    assert.equal(f.api.state.pending, reviewed); assert.equal(f.api.state.contents, page);
    f.api.catalogChanged({ household: 'S1', favorites: true, playlists: false });
    assert.equal(f.api.state.pending, reviewed);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    assert.equal(f.api.state.pending, null); assert.equal(f.api.state.contents, null);
    assert.equal(f.api.state.playlist, ''); assert.deepEqual(f.api.state.playlists, []);
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a catalog event fences an outstanding saved-track read', async () => {
    const f = await fixture(kind), held = deferred(); let waiting = false;
    f.read(async (url, read) => {
      const value = read(url);
      if (url.includes('/contents?')) { waiting = true; await held.promise; }
      return value;
    });
    const task = f.api.loadContents(); await until(() => waiting);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    held.resolve(); assert.equal(await task, false);
    assert.equal(f.api.state.contents, null); assert.equal(f.api.state.pending, null); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a catalog event during a submitted native track action preserves its one outcome', async () => {
    const f = await fixture(kind), reviewed = await f.review(), held = deferred();
    f.write(async (url, body, write) => { await held.promise; return write(url, body); });
    const task = f.confirm(reviewed); await until(() => f.writes().length === 1);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    assert.equal(f.api.state.contents, null); assert.equal(f.api.state.pending, null);
    held.resolve(); assert.equal(await task, true); assert.equal(f.api.state.result.status, 'accepted');
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 1);
  });

  for (const mode of ['play', 'next', 'end']) {
    test(`${kind}: ${mode} acts once on the second duplicate occurrence with native revision`, async () => {
      const f = await fixture(kind), reviewed = await f.review(2, mode);
      assert.ok(reviewed); assert.equal(f.writes().length, 0);
      assert.equal(reviewed.item.container, false); assert.equal(reviewed.item.position, 2);
      assert.ok(Object.isFrozen(reviewed.item)); assert.ok(Object.isFrozen(reviewed.body));
      assert.equal(await f.confirm(reviewed), true); assert.equal(await f.confirm(reviewed), false);
      assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sonos/playlists/track', body: {
        zone: 'Den@S1', playlist: 'SQ:7', position: 2, expected_revision: 'native:1', mode: mode === 'end' ? 'append' : mode,
      } }]);
      assert.match(f.api.state.result.detail, /Policy applied/);
      const exact = f.calls.filter(call => call.url.includes('/contents?') && new URL(call.url, 'http://localhost').searchParams.has('expected_revision'));
      assert.equal(exact.length, 2, 'recheck the exact saved occurrence for review and dispatch');
      for (const call of exact) {
        const query = new URL(call.url, 'http://localhost').searchParams;
        assert.equal(query.get('offset'), '1'); assert.equal(query.get('limit'), '1'); assert.equal(query.get('expected_revision'), 'native:1');
      }
      assert.equal(f.rows.get('Den@S1').transport_state, 'paused', 'the browser never invents a playback state');
    });
  }

  test(kind + ': an unavailable or unobserved occurrence cannot create a review or request', async () => {
    const f = await fixture(kind), count = f.calls.length;
    for (const position of [0, 3, 4, -1, 1.5, 100001]) {
      assert.equal(await f.review(position), null); assert.equal(f.calls.length, count);
    }
    assert.equal(f.writes().length, 0);
  });

  test(kind + ': changed revision invalidates selection both before review and before dispatch', async () => {
    const f = await fixture(kind); f.revision('native:2');
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 0);
    await f.api.loadContents(); const reviewed = await f.review(); assert.ok(reviewed);
    f.revision('native:3'); assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.writes().length, 0); assert.equal(f.api.state.result?.status === 'uncertain', false);
  });

  test(kind + ': replacing one same-position resource or title cannot silently substitute music', async () => {
    for (const field of ['uri', 'title']) {
      const f = await fixture(kind), reviewed = await f.review();
      f.entries[1][field] = field === 'uri' ? 'x-file-cifs://example.invalid/other.flac' : 'Different occurrence';
      assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    }
  });

  test(kind + ': an old row handler cannot select an occurrence from a refreshed page', async () => {
    const f = await fixture(kind), old = f.api.state.contents;
    await f.api.loadContents(); const before = f.calls.length;
    assert.equal(await f.review(2, 'next', old), null); assert.equal(f.calls.length, before);
    assert.ok(await f.review(2, 'next', f.api.state.contents));
  });

  test(kind + ': a changed household retires the reviewed native ID', async () => {
    const f = await fixture(kind), reviewed = await f.review(); f.selectOtherHousehold();
    assert.equal(f.api.state.pending, null); assert.equal(f.api.state.contents, null);
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a changed destination queue blocks insertion before sending', async () => {
    const f = await fixture(kind), reviewed = await f.review(2, 'next');
    f.queues.get('Den@S1').revision += ':changed';
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': late saved-track reads cannot revive closed or edited selections', async () => {
    for (const change of ['close', 'playlist']) {
      const f = await fixture(kind), gate = deferred(); let waiting = false;
      f.read(async (url, read) => {
        const value = read(url);
        if (url.includes('/contents?')) { waiting = true; await gate.promise; }
        return value;
      });
      const task = f.review(); await until(() => waiting);
      if (change === 'close') f.api.close(); else f.api.edit('playlist', 'SQ:8');
      gate.resolve(); assert.equal(await task, null); assert.equal(f.api.state.pending, null); assert.equal(f.writes().length, 0);
    }
  });

  test(kind + ': lost replies are uncertain and the same reviewed track is never replayed', async () => {
    const f = await fixture(kind), reviewed = await f.review(2, 'end');
    f.write(() => { throw new Error('The reply was lost'); });
    assert.equal(await f.confirm(reviewed), false); assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.api.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
  });

  test(kind + ': mismatched receipts do not claim a selected-track success', async () => {
    for (const patch of [{ playlist: 'SQ:8' }, { position: 1 }, { count: 3 }, { mode: 'play' }, { title: 'Other' }, { zone: 'Other' }]) {
      const f = await fixture(kind), reviewed = await f.review(2, 'next');
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      assert.equal(await f.confirm(reviewed), false); assert.equal(f.api.state.result.status, 'uncertain');
      assert.equal(f.writes().length, 1);
    }
  });

  test(kind + ': repeated clicks and close/reopen cannot duplicate an in-flight track action', async () => {
    const f = await fixture(kind), reviewed = await f.review(), gate = deferred();
    f.write(async (url, body, write) => { await gate.promise; return write(url, body); });
    const writing = f.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(await f.confirm(reviewed), false); f.api.close();
    if (kind === 'phone') await f.api.refresh(); else f.api.open();
    assert.equal(await f.confirm(reviewed), false); gate.resolve();
    assert.equal(await writing, true); assert.equal(f.writes().length, 1); assert.equal(f.api.state.result.status, 'accepted');
  });
}

async function reviewSelection(f, mode = 'play', positions = [2, 1]) {
  for (const position of positions) assert.equal(f.api.selectTrack(position), true);
  await f.api.reviewTracks(mode);
  return f.api.state.pending;
}

for (const kind of ['phone', 'dashboard']) {
  test(kind + ': displayed selection controls review the ordered native occurrences before one command', async () => {
    const f = await fixture(kind, { dom: true });
    f.entries[1].title = '<img src=x onerror=alert(1)> Second'; await f.api.loadContents();
    const all = () => kind === 'phone' ? descendants(f.api.node)
      : descendants(f.doc.getElementById('source-contents'));
    for (const position of [2, 1]) {
      const select = all().find(node => node.getAttribute?.('data-playlist-select') === String(position));
      assert.ok(select); assert.equal(select.disabled, false); select.focus(); select.fire('click');
      assert.equal(f.doc.activeElement.getAttribute('data-playlist-select'), String(position), 'selection preserves keyboard focus');
    }
    const panel = all().find(node => node.getAttribute?.('aria-label') === 'Selected saved tracks');
    const order = panel.children.find(node => node.getAttribute?.('aria-label') === 'Selected tracks in playback order');
    assert.deepEqual(order.children.map(node => node.textContent), ['Track 2: <img src=x onerror=alert(1)> Second', 'Track 1: First occurrence']);
    const play = descendants(panel).find(node => node.tag === 'button' && node.textContent === 'Play selected now');
    play.fire('click'); await until(() => !!f.api.state.pending);
    assert.equal(f.writes().length, 0);
    const preview = kind === 'phone' ? descendants(f.api.node).find(node => node.getAttribute?.('aria-label') === 'Review selected playback')
      : f.doc.getElementById('source-preview');
    const reviewed = kind === 'phone' ? descendants(preview).find(node => node.getAttribute?.('aria-label') === 'Reviewed saved tracks in playback order')
      : f.doc.getElementById('source-preview-selection');
    assert.deepEqual(reviewed.children.map(node => node.textContent), order.children.map(node => node.textContent));
    const confirm = kind === 'phone' ? descendants(preview).find(node => node.tag === 'button' && node.textContent === 'Play now')
      : f.doc.getElementById('source-confirm');
    assert.equal(confirm.disabled, false); confirm.fire('click'); await until(() => f.api.state.result?.status === 'accepted');
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/sonos/playlists/tracks');
    assert.deepEqual(f.writes()[0].body.positions, [2, 1]); assert.equal(all().some(node => node.tag === 'img'), false);
  });

  for (const mode of ['play', 'next', 'end']) test(`${kind}: ${mode} sends one ordered native selection with both source and queue revisions`, async () => {
    const f = await fixture(kind), reviewed = await reviewSelection(f, mode);
    assert.ok(reviewed); assert.equal(f.writes().length, 0);
    assert.deepEqual(reviewed.item.positions, [2, 1]); assert.equal(reviewed.item.tracks[0].uri, reviewed.item.tracks[1].uri);
    assert.ok(Object.isFrozen(reviewed.item.positions)); assert.ok(Object.isFrozen(reviewed.item.tracks));
    assert.equal(reviewed.before.queue.revision, 'queue:S1');
    assert.equal(await f.confirm(reviewed), true); assert.equal(await f.confirm(reviewed), false);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sonos/playlists/tracks', body: {
      zone: 'Den@S1', playlist: 'SQ:7', positions: [2, 1], expected_revision: 'native:1', expected_queue_revision: 'queue:S1',
      mode: mode === 'end' ? 'append' : mode,
    } }]);
    assert.match(f.api.state.result.detail, /Policy applied/);
    const reads = f.calls.filter(({ url }) => url.includes('/contents?') && new URL(url, 'http://localhost').searchParams.has('expected_revision'));
    assert.equal(reads.length, 2, 'one bounded selected range read for review and dispatch');
    for (const { url } of reads) {
      const query = new URL(url, 'http://localhost').searchParams;
      assert.equal(query.get('offset'), '0'); assert.equal(query.get('limit'), '2'); assert.equal(query.get('expected_revision'), 'native:1');
    }
    const effect = Dashboard.effect(reviewed);
    assert.match(effect, /order shown/); assert.match(effect, /saved playlist remains unchanged/);
    assert.match(effect, mode === 'play' ? /shuffle and repeat off/ : /without starting or resuming/);
  });

  test(kind + ': choosing tracks changes only the local selection and invalidates previous consent', async () => {
    const f = await fixture(kind), before = f.calls.length;
    assert.equal(f.api.selectTrack(2), true); assert.equal(f.api.selectTrack(1), true);
    assert.deepEqual(f.api.state.contentsSelection, [2, 1]); assert.equal(f.calls.length, before);
    await f.api.reviewTracks('play'); const reviewed = f.api.state.pending; assert.ok(reviewed);
    assert.equal(f.api.selectTrack(2), true); assert.equal(f.api.state.pending, null);
    assert.deepEqual(f.api.state.contentsSelection, [1]); assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.api.selectTrack(2), true); assert.deepEqual(f.api.state.contentsSelection, [1, 2]);
    assert.equal(f.api.clearTrackSelection(), true); assert.deepEqual(f.api.state.contentsSelection, []);
    assert.equal(f.writes().length, 0);
  });

  test(kind + ': the main source review uses the selection until it is explicitly cleared', async () => {
    const f = await fixture(kind);
    f.api.selectTrack(2); f.api.selectTrack(1); f.api.edit('mode', 'end');
    await f.api.review(); const reviewed = f.api.state.pending;
    assert.equal(reviewed.item.kind, 'sonos_playlist_tracks'); assert.deepEqual(reviewed.body.positions, [2, 1]);
    assert.equal(reviewed.body.mode, 'append'); assert.equal(reviewed.body.expected_queue_revision, 'queue:S1');
    assert.equal(f.writes().length, 0);
    f.api.clearTrackSelection(); assert.equal(await f.confirm(reviewed), false);
    await f.api.review(); assert.equal(f.api.state.pending.item.kind, 'sonos_playlist');
    assert.equal(f.writes().length, 0);
  });

  test(kind + ': native selection is bounded to ten playable observed occurrences', async () => {
    const f = await fixture(kind), before = f.calls.length;
    for (const position of [0, 3, 4, -1, 1.5, 100001]) assert.equal(f.api.selectTrack(position), false);
    assert.equal(f.calls.length, before); assert.deepEqual(f.api.state.contentsSelection, []);
    f.entries.splice(0, f.entries.length, ...Array.from({ length: 12 }, (_, index) => ({ title: 'Track ' + (index + 1), uri: 'fixture:track:' + index })));
    await f.api.loadContents();
    for (let position = 1; position <= 10; ++position) assert.equal(f.api.selectTrack(position), true);
    assert.equal(f.api.selectTrack(11), false); assert.equal(f.api.state.contentsSelection.length, 10);
    assert.equal(f.api.selectTrack(4), true); assert.equal(f.api.selectTrack(11), true);
    assert.deepEqual(f.api.state.contentsSelection, [1, 2, 3, 5, 6, 7, 8, 9, 10, 11]);
    await f.api.reviewTracks('next'); assert.ok(f.api.state.pending); assert.equal(f.writes().length, 0);
  });

  test(kind + ': selection is retired on refresh, page navigation, household changes and native invalidation', async () => {
    for (const change of ['refresh', 'page', 'household', 'catalog']) {
      const f = await fixture(kind);
      if (change === 'page') {
        f.entries.push(...Array.from({ length: 110 }, (_, index) => ({ title: 'Later ' + index, uri: 'fixture:later:' + index })));
        await f.api.loadContents();
      }
      const page = f.api.state.contents, reviewed = await reviewSelection(f);
      if (change === 'household') f.selectOtherHousehold();
      else if (change === 'catalog') f.api.catalogChanged({ household: 'S1', playlists: true });
      else await f.api.loadContents(change === 'page' ? 'next' : 'refresh');
      assert.deepEqual(f.api.state.contentsSelection, []); assert.equal(f.api.state.pending, null);
      assert.equal(f.api.selectTrack(2, page), false); assert.equal(await f.confirm(reviewed), false);
      assert.equal(f.writes().length, 0);
    }
  });

  test(kind + ': source changes and same-position substitutions prevent the whole selection', async () => {
    for (const change of [f => f.revision('native:2'), f => { f.entries[0].uri = 'fixture:other'; },
      f => { f.entries[1].title = 'Different title'; }, f => { f.entries.push({ title: 'Unexpected', uri: 'fixture:new' }); }]) {
      const f = await fixture(kind), reviewed = await reviewSelection(f); change(f);
      assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
      assert.notEqual(f.api.state.result?.status, 'uncertain');
    }
  });

  for (const mode of ['play', 'next', 'end']) test(`${kind}: ${mode} refuses a changed queue before dispatching the selection`, async () => {
    const f = await fixture(kind), reviewed = await reviewSelection(f, mode);
    f.queues.get('Den@S1').revision += ':replacement';
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': append selection permits track advance while play and next retain the reviewed occurrence', async () => {
    for (const mode of ['play', 'next', 'end']) {
      const f = await fixture(kind), reviewed = await reviewSelection(f, mode);
      f.queues.get('Den@S1').playing = 2; f.rows.get('Den@S1').track.queue_position = 2;
      assert.equal(await f.confirm(reviewed), mode === 'end');
      assert.equal(f.writes().length, mode === 'end' ? 1 : 0);
    }
  });

  test(kind + ': stale selections and closed reviews cannot return from late native reads', async () => {
    const f = await fixture(kind), held = deferred(); let waiting = false;
    f.api.selectTrack(2); f.api.selectTrack(1);
    f.read(async (url, read) => { const value = read(url); if (url.includes('/contents?')) { waiting = true; await held.promise; } return value; });
    const task = f.api.reviewTracks('play'); await until(() => waiting); f.api.close(); held.resolve(); await task;
    assert.equal(f.api.state.pending, null); assert.equal(f.writes().length, 0);
  });

  test(kind + ': uncertain batch replies retain the outcome and never replay a subset', async () => {
    for (const patch of [null, { positions: [1, 2] }, { titles: ['First occurrence', 'Second occurrence'] }, { count: 1 },
      { playlist: 'SQ:8' }, { mode: 'play' }, { zone: 'Other' }, { changed: null }, { positions: [2] }]) {
      const f = await fixture(kind), reviewed = await reviewSelection(f, 'next');
      f.write((url, body, write) => { if (patch === null) throw new Error('Lost reply'); return { ...write(url, body), ...patch }; });
      assert.equal(await f.confirm(reviewed), false); assert.equal(await f.confirm(reviewed), false);
      assert.equal(f.api.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
      assert.equal(f.writes()[0].url, '/sonos/playlists/tracks');
    }
  });

  test(kind + ': an admitted selection preserves its one receipt across closing and catalog invalidation', async () => {
    const f = await fixture(kind), reviewed = await reviewSelection(f), held = deferred();
    f.write(async (url, body, write) => { await held.promise; return write(url, body); });
    const writing = f.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(f.api.selectTrack(1), false); assert.equal(await f.confirm(reviewed), false);
    f.api.close(); f.api.catalogChanged({ household: 'S1', playlists: true });
    held.resolve(); assert.equal(await writing, true); assert.equal(f.api.state.result.status, 'accepted');
    assert.equal(f.writes().length, 1); assert.equal(await f.confirm(reviewed), false);
  });
}

test('an explicit native selection retains repeated positions and rejects an empty or oversized batch', () => {
  const page = { playlist: { id: 'SQ:7', title: 'Saved' }, revision: 'native:1', total: 2,
    items: [{ position: 1, title: 'First', uri: 'fixture:one' }, { position: 2, title: 'Second', uri: 'fixture:two' }] };
  const prepared = Phone.preparePlaylistTracks(page, [2, 1, 2], 'end');
  assert.deepEqual(Phone.commandBody(prepared, 'Den@S1'), {
    zone: 'Den@S1', playlist: 'SQ:7', positions: [2, 1, 2], expected_revision: 'native:1', mode: 'append',
  });
  for (const positions of [[], Array(11).fill(1), [3], [0], [1.1]]) assert.throws(() => Phone.preparePlaylistTracks(page, positions, 'play'));
});

test('row actions retain exact occurrence and render track metadata as text', async () => {
  const doc = { createElement(tag) { const handlers = new Map(); return { tag, children: [], textContent: '', disabled: false,
    append(node) { this.children.push(node); }, setAttribute(name, value) { this[name] = value; },
    addEventListener(name, handler) { handlers.set(name, handler); }, click() { handlers.get('click')?.(); } }; } };
  const list = { children: [], replaceChildren() { this.children = []; }, append(node) { this.children.push(node); } };
  const page = { items: [{ position: 52, title: '<img onerror=alert(1)>', uri: 'fixture:track' }, { position: 53, title: 'Unavailable' }] };
  const actions = [];
  Phone.renderContents(list, page, doc, { review: (...args) => actions.push(args) });
  assert.equal(list.children[0].children[0].textContent, '<img onerror=alert(1)>');
  const buttons = list.children[0].children.at(-1).children;
  assert.deepEqual(buttons.map(button => button.textContent), ['Play now', 'Play next', 'Add to queue']);
  buttons[1].click(); assert.deepEqual(actions, [[52, 'next', page]]);
  assert.ok(list.children[1].children.at(-1).children.every(button => button.disabled));
  Phone.renderContents(list, page, doc, { review: (...args) => actions.push(args), disabled: true });
  list.children[0].children.at(-1).children[0].click(); assert.equal(actions.length, 1);
});

for (const kind of ['phone', 'dashboard']) {
  async function selectedDestinations(mode = 'play', options = {}) {
    const f = await fixture(kind, { destinations: true, ...options });
    f.api.selectTrack(2); f.api.selectTrack(1);
    assert.equal(f.api.edit('selectionTarget', 'downstairs'), true);
    await f.api.reviewTracks(mode);
    assert.ok(f.api.state.pending, f.api.state.message);
    return f;
  }

  for (const mode of ['play', 'next', 'end']) {
    test(`${kind}: selected native ${mode} reaches independent groups in one request from the original source`, async () => {
      const f = await selectedDestinations(mode), reviewed = f.api.state.pending;
      assert.equal(f.writes().length, 0);
      assert.equal(reviewed.targets.groups.length, 2);
      assert.equal(reviewed.targets.sourceGroup.target, 'Den@S1');
      assert.deepEqual(reviewed.body, { zone: 'Den@S1', playlist: 'SQ:7', positions: [2, 1],
        expected_revision: 'native:1', mode: mode === 'end' ? 'append' : mode,
        target: 'downstairs', expected_target_revision: 'a'.repeat(64) });
      assert.equal(Object.hasOwn(reviewed.body, 'expected_queue_revision'), false);
      assert.equal(await f.confirm(reviewed), true);
      assert.equal(await f.confirm(reviewed), false);
      assert.equal(f.writes().length, 1);
      assert.equal(f.writes()[0].url, '/sonos/playlists/tracks');
      assert.equal(f.api.state.result.complete, true);
      assert.match(f.api.state.result.detail, /Den@S1/);
      assert.match(f.api.state.result.detail, /Office@S1/);
      const sourceReads = f.calls.filter(call => call.url.includes('/contents?'));
      assert.ok(sourceReads.length >= 3);
      assert.ok(sourceReads.every(call => new URL(call.url, 'http://localhost').searchParams.get('zone') === 'Den@S1'));
      if (kind === 'phone') assert.deepEqual(f.lanes[0].map(group => group.target).sort(), ['Den@S1', 'Office@S1']);
      if (mode !== 'play') assert.equal(f.api.state.result.undoable, true);
    });
  }

  test(kind + ': another single destination binds its queue while retaining the original saved source', async () => {
    const f = await selectedDestinations('next');
    f.api.edit('selectionTarget', 'Office@S1');
    await f.api.reviewTracks('next');
    const reviewed = f.api.state.pending;
    assert.equal(reviewed.body.zone, 'Den@S1');
    assert.equal(reviewed.body.target, 'Office@S1');
    assert.equal(reviewed.body.expected_queue_revision, 'queue:S1:Office');
    assert.equal(await f.confirm(), true);
    assert.equal(f.writes().length, 1);
    assert.match(f.api.state.result.detail, /Office@S1/);
  });

  test(kind + ': destination edits keep ordered selections and retire the old review', async () => {
    const f = await selectedDestinations(), prior = f.api.state.pending, page = f.api.state.contents;
    f.api.edit('selectionTarget', 'Office@S1');
    assert.equal(f.api.state.pending, null);
    assert.equal(f.api.state.contents, page);
    assert.deepEqual(f.api.state.contentsSelection, [2, 1]);
    assert.equal(await f.confirm(prior), false);
    assert.equal(f.writes().length, 0);
    f.api.edit('selectionTarget', '');
    await f.api.reviewTracks('end');
    assert.equal(f.api.state.pending.body.zone, 'Den@S1');
    assert.equal(Object.hasOwn(f.api.state.pending.body, 'target'), false, 'blank retains the established one-group request');
    assert.equal(await f.confirm(), true);
    assert.equal(f.writes().length, 1);
  });

  test(kind + ': selected tracks cannot cross from their source household', async () => {
    const f = await fixture(kind, { destinations: true });
    f.api.selectTrack(2); f.api.edit('selectionTarget', 'everywhere');
    await f.api.reviewTracks('play');
    assert.equal(f.api.state.pending, null);
    assert.match(f.api.state.message, /original household/);
    assert.equal(f.writes().length, 0);
    assert.deepEqual(f.api.state.contentsSelection, [2]);
  });

  test(kind + ': source, audience and destination queue changes each require a fresh native selection review', async () => {
    for (const change of ['source', 'audience', 'queue']) {
      const f = await selectedDestinations(), reviewed = f.api.state.pending;
      if (change === 'source') f.revision('native:2');
      if (change === 'queue') f.queues.get('Office@S1').revision = 'queue:changed';
      if (change === 'audience') f.read((url, read) => {
        const value = read(url);
        if (url.startsWith('/rooms/resolve?')) value.revision = 'b'.repeat(64);
        return value;
      });
      assert.equal(await f.confirm(reviewed), false, change);
      assert.equal(f.writes().length, 0, change);
      assert.equal(f.api.state.result.status, 'not_sent', change);
    }
  });

  test(kind + ': repeated saved occurrences retain their order across destinations', async () => {
    const f = await fixture(kind, { destinations: true });
    f.api.edit('selectionTarget', 'downstairs');
    await f.api.review(Phone.preparePlaylistTracks(f.api.state.contents, [2, 1, 2], 'end'));
    assert.deepEqual(f.api.state.pending.body.positions, [2, 1, 2]);
    assert.equal(await f.confirm(), true);
    assert.equal(f.writes().length, 1);
    assert.equal(f.api.state.result.complete, true);
  });

  test(kind + ': native batch playback shows partial receipts and never repeats the batch', async () => {
    const f = await selectedDestinations();
    f.write((url, body, write) => {
      const value = write(url, body); value.batch.complete = false;
      value.batch.results[1].result = { status: 'failed', error: { code: 'OUTCOME_UNCERTAIN',
        detail: 'The destination disconnected after dispatch.', retryable: false, hint: 'Read its playback before another action.' } };
      return value;
    });
    const reviewed = f.api.state.pending;
    assert.equal(await f.confirm(), true);
    assert.equal(f.api.state.result.complete, false);
    assert.equal(f.api.state.result.partial, true);
    assert.equal(f.api.state.result.uncertain, true);
    assert.match(f.api.state.result.detail, /Den@S1: Confirmed/);
    assert.match(f.api.state.result.detail, /Office@S1: Outcome uncertain/);
    assert.match(f.api.state.result.detail, /Do not repeat/);
    assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.writes().length, 1);
  });

  test(kind + ': native queue batches retain exact group ranges, failures and unavailable undo', async () => {
    const f = await selectedDestinations('next');
    f.write((url, body, write) => {
      const value = write(url, body), batch = value.queue_batch, failed = batch.added.pop();
      batch.complete = false; batch.undoable = false;
      batch.failed.push({ household: failed.household, coordinator: failed.coordinator, zone: 'Office',
        error: { code: 'QUEUE_CHANGED', detail: 'The Office queue changed before insertion.', retryable: false } });
      return value;
    });
    assert.equal(await f.confirm(), true);
    assert.equal(f.api.state.result.complete, false);
    assert.equal(f.api.state.result.undoable, false);
    assert.match(f.api.state.result.detail, /Den@S1/);
    assert.match(f.api.state.result.detail, /Office@S1: QUEUE_CHANGED/);
    assert.match(f.api.state.result.detail, /No safe batch undo/);
    assert.equal(f.writes().length, 1);
  });

  test(kind + ': partial native queue delivery to one alias retains the failed receipt', async () => {
    const f = await selectedDestinations('end');
    f.api.edit('selectionTarget', 'Office@S1'); await f.api.reviewTracks('end');
    f.write((url, body, write) => {
      const value = write(url, body), batch = value.queue_batch, failed = batch.added.pop();
      value.zone = body.target; value.changed = false;
      batch.complete = false; batch.undoable = false;
      batch.failed.push({ household: failed.household, coordinator: failed.coordinator, zone: 'Office',
        error: { code: 'QUEUE_CHANGED', detail: 'The queue changed before insertion.', retryable: false } });
      return value;
    });
    assert.equal(await f.confirm(), true);
    assert.equal(f.api.state.result.partial, true);
    assert.match(f.api.state.result.detail, /Office@S1: QUEUE_CHANGED/);
    assert.equal(f.writes().length, 1);
  });

  test(kind + ': a mismatched saved occurrence receipt remains uncertain after one dispatch', async () => {
    const f = await selectedDestinations('next');
    f.write((url, body, write) => { const value = write(url, body); value.positions.reverse(); return value; });
    assert.equal(await f.confirm(), false);
    assert.equal(f.api.state.result.status, 'uncertain');
    assert.equal(f.writes().length, 1);
  });

  test(kind + ': the shipped selection panel exposes the independent native destination', async () => {
    const f = await selectedDestinations('next', { dom: true });
    const root = kind === 'phone' ? f.api.node : f.doc.getElementById('source-contents');
    const input = descendants(root).find(node => node.getAttribute?.('aria-label') === 'Selected-track destination');
    assert.ok(input);
    assert.equal(input.value, 'downstairs');
    input.value = 'Office@S1'; input.fire('input');
    assert.equal(f.api.state.selectionTarget, 'Office@S1');
    assert.deepEqual(f.api.state.contentsSelection, [2, 1]);
    assert.equal(f.api.state.pending, null);
    await (kind === 'phone' ? f.api.loadTargetSuggestions() : f.api.loadTargets());
    const suggestions = descendants(root).find(node => node.getAttribute?.('aria-label') === 'Selected-track destination suggestions');
    assert.ok(suggestions.children.some(option => option.value === 'all@S1'));
    assert.ok(suggestions.children.some(option => option.value === 'downstairs'));
    assert.equal(suggestions.children.some(option => option.value === 'Den@S2'), false);
    assert.equal(suggestions.children.some(option => option.value === 'everywhere'), false);
  });
}
