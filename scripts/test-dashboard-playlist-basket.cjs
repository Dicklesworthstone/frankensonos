'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Player = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const copy = value => structuredClone(value);
const TARGET_REVISION = 'a'.repeat(64);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) {
  for (let i = 0; i < 400 && !check(); ++i) await Promise.resolve();
  assert.ok(check(), 'reached the asynchronous command boundary');
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
  doc.getElementById('source-contents').append(doc.getElementById('source-contents-items'));
  return doc;
}
function descendants(node) {
  return [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? descendants(child) : [])];
}

// Keep the native source separate from the reviewed destination groups.
// Pages contain repeated resources as distinct saved occurrences.
async function fixture({ household = 'S1', dom = false } = {}) {
  const foreign = household === 'S1' ? 'S2' : 'S1', origin = 'Den@' + household;
  const zones = [
    { household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' },
    { household, coordinator_room: 'Office', members: ['Office', 'Side'], transport_state: 'playing' },
    { household, coordinator_room: 'Bedroom', members: ['Bedroom'], transport_state: 'paused' },
    { household: foreign, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' },
  ];
  const rows = new Map(zones.map(zone => [Core.key(zone), { zone, transport_state: zone.transport_state, volume: 18, mute: false,
    track: { uri: 'fixture:current-' + Core.key(zone), title: 'Current music', queue_position: 2, position_secs: 24, duration_secs: 240 } }]));
  const queues = new Map(zones.map((zone, i) => [Core.key(zone), { zone: zone.coordinator_room, total: 6 + i,
    playing: zone.coordinator_room === 'Bedroom' ? null : 2, revision: 'queue:' + Core.key(zone),
    items: [{ position: 1, title: 'Retained song', uri: 'fixture:first' }] }]));
  const catalog = [{ id: 'SQ:7', title: 'Saved & repeated' }, { id: 'SQ:8', title: 'Other playlist' }];
  const entries = Array.from({ length: 240 }, (_, index) => ({
    title: 'Saved track ' + (index + 1), uri: 'x-file-cifs://example.invalid/' + (index < 2 ? 'repeated' : 'track-' + (index + 1)) + '.flac',
    creator: 'Fixture artist',
  }));
  let nativeRevision = 'native:1', reader = null, writer = null, source;
  const calls = [], completed = [];
  const player = (home, name) => 'fixture-' + home + '-' + name;
  const destinations = target => target === 'upstairs' ? zones.filter(z => z.household === household && z.coordinator_room !== 'Den')
    : target === 'everywhere' ? zones : target.startsWith('all@') ? zones.filter(z => z.household === target.slice(4))
      : zones.filter(z => Core.key(z) === target);
  const resolution = target => {
    const selected = destinations(target);
    const members = zone => zone.members.map(room => ({ household: zone.household, room,
      player: player(zone.household, room), coordinator: player(zone.household, zone.coordinator_room) }));
    return { target, revision: target === 'upstairs' ? TARGET_REVISION : 'b'.repeat(64),
      matched: target === 'upstairs' ? 'alias' : target === 'everywhere' || target.startsWith('all@') ? 'everywhere' : 'room',
      rooms: selected.flatMap(members),
      groups: selected.map(zone => ({ household: zone.household, coordinator: player(zone.household, zone.coordinator_room),
        coordinator_room: zone.coordinator_room, members: members(zone) })) };
  };
  const read = url => {
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/rooms/resolve') return resolution(query.target);
    if (parsed.pathname === '/rooms') return zones.flatMap(zone => zone.members.map(name => ({
      name, household: zone.household, zone: zone.coordinator_room, aliases: zone.coordinator_room === 'Den' ? [] : ['upstairs'],
    })));
    if (parsed.pathname === '/sonos/playlists') {
      assert.equal(query.zone, origin, 'read the catalog from the original source group');
      return copy(catalog);
    }
    if (parsed.pathname === '/sonos/playlists/contents') {
      assert.equal(query.zone, origin, 'reread saved occurrences at the source, never the destination');
      if (query.expected_revision && query.expected_revision !== nativeRevision) {
        throw Object.assign(new Error('The saved playlist changed. Read tracks again.'), { code: 'QUEUE_CHANGED' });
      }
      const offset = Number(query.offset), limit = Number(query.limit);
      assert.ok(limit >= 1 && limit <= 100, 'native reads stay within the public page bound');
      const items = entries.slice(offset, offset + limit).map((item, i) => ({ ...item, position: offset + i + 1 }));
      return { zone: 'Den', playlist: copy(catalog.find(item => item.id === query.playlist)), revision: nativeRevision,
        total: entries.length, offset, next_offset: offset + items.length < entries.length ? offset + items.length : null, items };
    }
    const target = decodeURIComponent(parsed.pathname.split('/')[2] || '');
    if (parsed.pathname.endsWith('/state')) return copy(rows.get(target));
    if (parsed.pathname.endsWith('/queue')) return copy(queues.get(target));
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.ok(['/sonos/playlists/tracks', '/sonos/playlists/track'].includes(url),
      'use the native occurrence endpoint, never raw URIs or a whole playlist');
    const many = url.endsWith('/tracks'), positions = many ? body.positions : [body.position];
    const selected = destinations(body.target || body.zone);
    const result = { done: 'Selected saved tracks delivered', changed: true, notes: [],
      zone: selected.length === 1 ? selected[0].coordinator_room : body.target, playlist: body.playlist, mode: body.mode,
      ...(many ? { positions: [...positions], titles: positions.map(position => entries[position - 1].title) }
        : { position: body.position, title: entries[body.position - 1].title }) };
    if (body.mode === 'play' && selected.length > 1) {
      result.batch = { complete: true, results: selected.map(zone => ({ household: zone.household,
        target: player(zone.household, zone.coordinator_room), zone: zone.coordinator_room,
        result: { status: 'succeeded', done: 'Selected tracks playing', changed: true, notes: [] } })) };
    } else if (body.mode !== 'play') {
      const added = selected.map(zone => {
        const queue = queues.get(Core.key(zone));
        return { household: zone.household, coordinator: player(zone.household, zone.coordinator_room),
          queue: { zone: zone.coordinator_room, done: 'Selected tracks queued',
            first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1,
            count: positions.length, playing_queue: queue.playing !== null } };
      });
      if (body.target) result.queue_batch = { done: 'Selected tracks queued', complete: true, added, failed: [], logged: true, undoable: true };
      if (added.length === 1) {
        const { first, count, playing_queue } = added[0].queue;
        Object.assign(result, { first, count, playing_queue });
      }
    }
    return result;
  };
  const call = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, read, options) : read(url)
      : writer ? writer(url, body, write) : write(url, body);
  };
  const controller = Core.model({ call, changed: () => source?.observe(), now: () => 1000 });
  const doc = dom ? documentFixture() : null;
  const options = { call, controller, completed: result => completed.push(result), now: () => 1000 };
  source = dom ? Dashboard.panel({ ...options, document: doc }) : Dashboard.model(options);
  assert.equal(await controller.refresh(), true);
  assert.equal(controller.select(origin), true); controller.connection(true); source.open();
  assert.equal(source.edit('sourceType', 'playlist'), true); assert.equal(await source.loadPlaylists(), true);
  assert.equal(source.edit('playlist', 'SQ:7'), true); assert.equal(await source.loadContents(), true);
  return { source, controller, calls, completed, rows, queues, zones, catalog, entries, resolution, origin, household, foreign, doc,
    writes: () => calls.filter(call => call.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, revision(value) { nativeRevision = value; },
    async review(mode = 'play', target = 'upstairs') {
      assert.equal(source.selectTrack(2), true);
      assert.equal(await source.loadContents('next'), true); assert.equal(source.selectTrack(102), true);
      assert.equal(await source.loadContents('next'), true); assert.equal(source.selectTrack(203), true);
      assert.equal(source.edit('selectionTarget', target), true);
      const reviewed = await source.reviewTracks(mode); assert.ok(reviewed, source.state.message); return reviewed;
    },
  };
}

for (const household of ['S1', 'S2']) for (const mode of ['play', 'next', 'end']) {
  test(`dashboard ${mode} retains three native pages for room sets in ${household}`, async () => {
    const f = await fixture({ household }), reviewed = await f.review(mode);
    assert.deepEqual(f.source.state.contentsSelection, [2, 102, 203]);
    assert.deepEqual(reviewed.item.positions, [2, 102, 203]);
    assert.equal(reviewed.targets.sourceGroup.target, f.origin);
    assert.deepEqual(reviewed.item.tracks.map(track => track.title), ['Saved track 2', 'Saved track 102', 'Saved track 203']);
    assert.equal(f.source.state.contents.offset, 200); assert.equal(f.writes().length, 0);
    assert.equal(await f.source.apply(reviewed), true);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sonos/playlists/tracks',
      body: { zone: f.origin, target: 'upstairs', expected_target_revision: TARGET_REVISION, playlist: 'SQ:7',
        positions: [2, 102, 203], expected_revision: 'native:1', mode: mode === 'end' ? 'append' : mode } }]);
    assert.equal(f.source.state.result.complete, true);
    assert.match(f.source.state.result.detail, /Office/); assert.match(f.source.state.result.detail, /Bedroom/);
    assert.deepEqual(f.completed[0].targets.sort(), ['Bedroom@' + household, 'Office@' + household]);
    assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 1);
    const observed = f.calls.filter(call => call.body === undefined && call.url.startsWith('/sonos/playlists/contents?'));
    for (const call of observed) {
      const query = new URL(call.url, 'http://localhost').searchParams;
      assert.equal(query.get('zone'), f.origin);
      assert.ok(Number(query.get('limit')) <= 100);
    }
    const exact = observed.filter(call => Number(new URL(call.url, 'http://localhost').searchParams.get('limit')) < 100);
    assert.ok(exact.length >= 6, 'review and dispatch each reread all three separated source ranges');
    assert.ok(exact.every(call => new URL(call.url, 'http://localhost').searchParams.get('expected_revision') === 'native:1'));
    assert.equal(f.rows.get(f.origin).transport_state, 'paused');
  });
}

for (const mode of ['play', 'next', 'end']) {
  test('dashboard ' + mode + ' rereads an off-page basket for the original single group', async () => {
    const f = await fixture(), reviewed = await f.review(mode, '');
    assert.equal(reviewed.targets, null); assert.deepEqual(reviewed.body.positions, [2, 102, 203]);
    assert.equal(reviewed.body.expected_queue_revision, f.queues.get(f.origin).revision);
    assert.equal(await f.source.apply(reviewed), true); assert.equal(f.writes().length, 1);
    assert.equal(f.writes()[0].body.target, undefined); assert.equal(f.writes()[0].body.zone, f.origin);
  });
}

test('next and previous preserve pinned occurrences while old-page row handlers expire', async () => {
  const f = await fixture(), first = f.source.state.contents;
  f.source.selectTrack(2); const basket = f.source.state.contentsBasket;
  assert.equal(await f.source.loadContents('next'), true);
  assert.equal(f.source.state.contentsBasket, basket); assert.deepEqual(f.source.state.contentsSelection, [2]);
  assert.equal(f.source.selectTrack(1, first), false);
  f.source.selectTrack(102); assert.equal(await f.source.loadContents('previous'), true);
  assert.deepEqual(f.source.state.contentsSelection, [2, 102]); assert.equal(f.source.state.contents.offset, 0);
  assert.equal(f.source.selectTrack(2), true); assert.deepEqual(f.source.state.contentsSelection, [102]);
  assert.equal(f.source.state.contentsBasket.group.target, f.origin); assert.equal(f.writes().length, 0);
});

test('reorder, repeat and remove preserve exact native occurrence order across pages', async () => {
  const f = await fixture(); await f.review('play');
  assert.equal(f.source.changeTrackSelection('up', 2), true);
  assert.equal(f.source.state.pending, null); assert.deepEqual(f.source.state.contentsSelection, [2, 203, 102]);
  assert.equal(f.source.changeTrackSelection('repeat', 0), true);
  assert.deepEqual(f.source.state.contentsSelection, [2, 2, 203, 102]);
  assert.equal(f.source.changeTrackSelection('down', 0), true);
  assert.equal(f.source.changeTrackSelection('remove', 0), true);
  assert.deepEqual(f.source.state.contentsSelection, [2, 203, 102]);
  assert.equal(f.source.changeTrackSelection('repeat', 2), true);
  assert.deepEqual(f.source.state.contentsSelection, [2, 203, 102, 102]);
  const reviewed = await f.source.reviewTracks('next');
  assert.ok(reviewed); assert.equal(await f.source.apply(reviewed), true);
  assert.deepEqual(f.writes()[0].body.positions, [2, 203, 102, 102]);
});

test('repeat counts toward ten tracks and unselecting a source row removes all its repeats', async () => {
  const f = await fixture(); f.source.selectTrack(2);
  for (let i = 1; i < 10; ++i) assert.equal(f.source.changeTrackSelection('repeat', 0), true);
  const basket = f.source.state.contentsBasket;
  assert.equal(f.source.changeTrackSelection('repeat', 0), false);
  assert.equal(f.source.selectTrack(3), false); assert.equal(f.source.state.contentsBasket, basket);
  assert.deepEqual(f.source.state.contentsSelection, Array(10).fill(2));
  assert.equal(f.source.changeTrackSelection('remove', 0), true); assert.equal(f.source.state.contentsSelection.length, 9);
  assert.equal(f.source.selectTrack(2), true); assert.equal(f.source.state.contentsBasket, null);
  assert.deepEqual(f.source.state.contentsSelection, []); assert.equal(f.writes().length, 0);
});

test('retained basket controls cannot edit or review a newer selection', async () => {
  const f = await fixture(); f.source.selectTrack(2); f.source.selectTrack(1);
  const page = f.source.state.contents, original = f.source.state.contentsBasket;
  assert.equal(f.source.changeTrackSelection('up', 1, original), true);
  const current = f.source.state.contentsBasket;
  for (const action of ['up', 'down', 'remove', 'repeat']) assert.equal(f.source.changeTrackSelection(action, 0, original), false);
  assert.equal(await f.source.reviewTracks('play', page, original), null);
  assert.equal(f.source.state.contentsBasket, current); assert.deepEqual(f.source.state.contentsSelection, [1, 2]);
  assert.equal(f.writes().length, 0);
});

test('destination edits keep customized basket order and retire the prior review', async () => {
  const f = await fixture(); await f.review('play');
  f.source.changeTrackSelection('up', 2); f.source.changeTrackSelection('repeat', 0);
  const reviewed = await f.source.reviewTracks('next'), basket = f.source.state.contentsBasket;
  assert.equal(f.source.edit('selectionTarget', 'Office@S1'), true);
  assert.equal(f.source.state.contentsBasket, basket); assert.deepEqual(f.source.state.contentsSelection, [2, 2, 203, 102]);
  assert.equal(f.source.state.pending, null); assert.equal(await f.source.apply(reviewed), false);
  const changed = await f.source.reviewTracks('next'); assert.equal(changed.body.target, 'Office@S1');
  assert.equal(await f.source.apply(changed), true); assert.deepEqual(f.writes()[0].body.positions, [2, 2, 203, 102]);
});

test('a failed next-page read keeps the prior page and basket available for a fresh review', async () => {
  const f = await fixture(); f.source.selectTrack(2); await f.source.loadContents('next'); f.source.selectTrack(102);
  const page = f.source.state.contents, basket = f.source.state.contentsBasket;
  const reviewed = await f.source.reviewTracks('next');
  f.read((url, read) => {
    if (url.includes('/contents?') && new URL(url, 'http://localhost').searchParams.get('offset') === '200') throw new Error('Temporary browse failure');
    return read(url);
  });
  assert.equal(await f.source.loadContents('next'), false);
  assert.equal(f.source.state.contents, page); assert.equal(f.source.state.contentsBasket, basket);
  assert.deepEqual(f.source.state.contentsSelection, [2, 102]); assert.equal(f.source.state.pending, null);
  assert.match(f.source.state.message, /Temporary browse failure/); assert.equal(await f.source.apply(reviewed), false);
  assert.ok(await f.source.reviewTracks('next')); assert.equal(f.writes().length, 0);
});

test('a daemon revision refusal clears an off-page basket until explicit refresh', async () => {
  const f = await fixture(); f.source.selectTrack(2);
  await f.source.loadContents('next'); f.source.selectTrack(102); f.revision('native:2');
  assert.equal(await f.source.loadContents('next'), false);
  assert.equal(f.source.state.contentsBasket, null); assert.equal(f.source.state.contents, null);
  assert.deepEqual(f.source.state.contentsSelection, []); assert.equal(await f.source.loadContents('next'), false);
  assert.equal(await f.source.loadContents(), true); assert.equal(f.source.state.contents.revision, 'native:2');
  assert.equal(f.writes().length, 0);
});

for (const conflict of ['revision', 'total', 'title', 'selected resource']) {
  test('a returned page with changed ' + conflict + ' discards the pinned native basket', async () => {
    const f = await fixture(); f.source.selectTrack(2); await f.source.loadContents('next'); f.source.selectTrack(102);
    f.read((url, read) => {
      const value = read(url);
      if (url.includes('/contents?')) {
        if (conflict === 'revision') value.revision = 'native:2';
        if (conflict === 'total') value.total += 1;
        if (conflict === 'title') value.playlist.title = 'Changed saved playlist';
        if (conflict === 'selected resource') value.items.find(track => track.position === 2).uri = 'fixture:replacement';
      }
      return value;
    });
    assert.equal(await f.source.loadContents('previous'), false);
    assert.equal(f.source.state.contentsBasket, null); assert.equal(f.source.state.contents, null);
    assert.deepEqual(f.source.state.contentsSelection, []); assert.equal(await f.source.reviewTracks('play'), null);
    assert.equal(f.writes().length, 0);
  });
}

for (const change of ['catalog', 'group', 'playlist', 'source']) {
  test('a late page cannot restore a basket after a changed ' + change, async () => {
    const f = await fixture(), held = deferred(); let waiting = false;
    f.source.selectTrack(2);
    f.read(async (url, read) => {
      const value = read(url);
      if (url.includes('/contents?')) { waiting = true; await held.promise; }
      return value;
    });
    const task = f.source.loadContents('next'); await until(() => waiting);
    if (change === 'catalog') f.source.catalogChanged({ household: f.household, playlists: true });
    if (change === 'group') f.controller.select('Den@' + f.foreign);
    if (change === 'playlist') f.source.edit('playlist', 'SQ:8');
    if (change === 'source') f.source.edit('sourceType', 'link');
    held.resolve(); assert.equal(await task, false);
    assert.equal(f.source.state.contentsBasket, null); assert.equal(f.source.state.contents, null);
    assert.deepEqual(f.source.state.contentsSelection, []); assert.equal(f.writes().length, 0);
  });
}

test('explicit track refresh clears selection before the replacement page arrives', async () => {
  const f = await fixture(), held = deferred(); let waiting = false;
  f.source.selectTrack(2);
  f.read(async (url, read) => {
    const value = read(url);
    if (url.includes('/contents?')) { waiting = true; await held.promise; }
    return value;
  });
  const task = f.source.loadContents('refresh'); await until(() => waiting);
  assert.equal(f.source.state.contentsBasket, null); assert.deepEqual(f.source.state.contentsSelection, []);
  held.resolve(); assert.equal(await task, true);
  assert.equal(f.source.state.contentsBasket, null); assert.deepEqual(f.source.state.contentsSelection, []);
});

for (const moment of ['review', 'dispatch']) {
  test('an off-page native source change blocks the basket at ' + moment, async () => {
    const f = await fixture(); const reviewed = await f.review('next');
    f.entries[1].uri = 'x-file-cifs://example.invalid/replaced.flac';
    if (moment === 'review') {
      f.source.cancelReview(); assert.equal(await f.source.reviewTracks('next'), null);
    } else {
      assert.equal(await f.source.apply(reviewed), false); assert.equal(f.source.state.result.status, 'not_sent');
    }
    assert.equal(f.writes().length, 0); assert.match(f.source.state.message, /changed|again/i);
  });
}

test('the dashboard command lane covers an entire cross-page native basket through its receipt', async () => {
  const f = await fixture(), reviewed = await f.review('next'), held = deferred();
  f.write(async (url, body, write) => { await held.promise; return write(url, body); });
  const task = f.source.apply(reviewed); await until(() => f.writes().length === 1);
  assert.equal(f.controller.state.busy, true); assert.equal(f.source.changeTrackSelection('repeat', 0), false);
  assert.equal(await f.source.loadContents('previous'), false); assert.equal(await f.controller.command('resume'), false);
  f.source.close(); held.resolve(); assert.equal(await task, true);
  assert.deepEqual(f.writes()[0].body.positions, [2, 102, 203]); assert.equal(await f.source.apply(reviewed), false);
  assert.equal(f.writes().length, 1); assert.equal(f.source.state.result.complete, true);
});

test('dashboard DOM retains off-page selections and binds reorder controls to their displayed basket', async () => {
  const f = await fixture({ dom: true }), byId = id => f.doc.getElementById(id);
  const choose = position => descendants(byId('source-contents-items'))
    .find(node => node.getAttribute?.('data-playlist-select') === String(position)).fire('click');
  const list = () => descendants(byId('source-contents')).find(node => node.className === 'source-selected-tracks');
  const action = (name, index) => descendants(list()).find(node =>
    node.getAttribute?.('data-playlist-basket-action') === name && node.getAttribute?.('data-playlist-basket-index') === String(index));
  choose(2); const oldRemove = action('remove', 0);
  byId('source-contents-next').fire('click'); await until(() => f.source.state.contents?.offset === 100 && !f.source.state.loading);
  assert.deepEqual(list().children.map(row => row.textContent), ['Track 2: Saved track 2']);
  choose(102); assert.deepEqual(list().children.map(row => row.textContent), ['Track 2: Saved track 2', 'Track 102: Saved track 102']);
  oldRemove.fire('click'); assert.deepEqual(f.source.state.contentsSelection, [2, 102], 'old row control cannot remove from a newer basket');
  action('up', 1).fire('click'); assert.deepEqual(f.source.state.contentsSelection, [102, 2]);
  action('repeat', 0).fire('click'); assert.deepEqual(f.source.state.contentsSelection, [102, 102, 2]);
  action('remove', 1).fire('click'); assert.deepEqual(f.source.state.contentsSelection, [102, 2]);
  const selection = descendants(byId('source-contents')).find(node => node.className === 'source-track-selection');
  descendants(selection).find(node => node.tag === 'button' && node.textContent === 'Play selected now').fire('click');
  await until(() => !!f.source.state.pending);
  assert.deepEqual(byId('source-preview-selection').children.map(row => row.textContent), ['Track 102: Saved track 102', 'Track 2: Saved track 2']);
  byId('source-confirm').fire('click'); await until(() => f.source.state.result && !f.source.state.writing);
  assert.equal(f.source.state.result.status, 'accepted'); assert.deepEqual(f.writes()[0].body.positions, [102, 2]);
  assert.equal(f.writes().length, 1);
});
