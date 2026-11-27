'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const copy = value => structuredClone(value);
const REVISION = 'a'.repeat(64);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) {
  for (let i = 0; i < 500 && !check(); ++i) await Promise.resolve();
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
  doc.getElementById('source-online').append(doc.getElementById('source-catalog-items'));
  return doc;
}
const descendants = node => [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? descendants(child) : [])];
const uri = (kind, index) => `spotify:${kind}:${String(index).padStart(22, '0')}`;

async function fixture({ dom = false } = {}) {
  const origin = 'Den@S2', zones = [
    { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Side'], transport_state: 'playing' },
    { household: 'S2', coordinator_room: 'Office', members: ['Office'], transport_state: 'playing' },
    { household: 'S1', coordinator_room: 'Bedroom', members: ['Bedroom'], transport_state: 'paused' },
  ];
  const rows = new Map(zones.map(zone => [Core.key(zone), { zone, transport_state: zone.transport_state, volume: 18, mute: false,
    track: { uri: 'fixture:current-' + Core.key(zone), title: 'Current music', queue_position: 2, position_secs: 24, duration_secs: 240 } }]));
  const queues = new Map(zones.map((zone, index) => [Core.key(zone), { zone: zone.coordinator_room, total: 6 + index,
    playing: zone.coordinator_room === 'Bedroom' ? null : 2, revision: 'queue:' + Core.key(zone),
    items: [{ position: 1, title: 'Retained song', uri: 'fixture:first' }] }]));
  const lists = new Map([
    ['Calm|track', Array.from({ length: 23 }, (_, index) => ({ source_uri: uri('track', index + 1), title: 'Catalog song ' + (index + 1), subtitle: 'Fixture performer' }))],
    ['Talks|episode', Array.from({ length: 12 }, (_, index) => ({ source_uri: uri('episode', index + 101), title: 'Catalog episode ' + (index + 1), subtitle: 'Fixture show' }))],
    ['Albums|album', [{ source_uri: uri('album', 501), title: 'Uncached album', subtitle: 'Fixture performer' }]],
  ]);
  let reader = null, writer = null, source, revision = REVISION, time = 1000;
  const calls = [], completed = [], player = (home, room) => 'fixture-' + home + '-' + room;
  const destinations = target => target === 'away' ? zones.filter(zone => Core.key(zone) !== origin)
    : target === 'everywhere' ? zones : zones.filter(zone => Core.key(zone) === target);
  const resolution = target => {
    const selected = destinations(target), members = zone => zone.members.map(room => ({ household: zone.household, room,
      player: player(zone.household, room), coordinator: player(zone.household, zone.coordinator_room) }));
    return { target, revision, matched: target === 'away' ? 'alias' : target === 'everywhere' ? 'everywhere' : 'room',
      rooms: selected.flatMap(members), groups: selected.map(zone => ({ household: zone.household,
        coordinator: player(zone.household, zone.coordinator_room), coordinator_room: zone.coordinator_room, members: members(zone) })) };
  };
  const read = url => {
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/rooms/resolve') return resolution(query.target);
    if (parsed.pathname === '/rooms') return zones.flatMap(zone => zone.members.map(name => ({
      name, household: zone.household, zone: zone.coordinator_room, aliases: Core.key(zone) === origin ? [] : ['away'],
    })));
    if (parsed.pathname === '/spotify/search') {
      const entries = lists.get(query.q + '|' + query.kind) || [], offset = Number(query.offset || 0), limit = Number(query.limit);
      assert.ok(Number.isSafeInteger(limit) && limit >= 1 && limit <= 10, 'online reads stay bounded');
      const items = copy(entries.slice(offset, offset + limit)), next = offset + items.length;
      return { query: query.q, kind: query.kind, offset, items, more: next < entries.length,
        next_offset: next < entries.length ? next : null };
    }
    const target = decodeURIComponent(parsed.pathname.split('/')[2] || '');
    if (parsed.pathname.endsWith('/state')) return copy(rows.get(target));
    if (parsed.pathname.endsWith('/queue')) return copy(queues.get(target));
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.equal(url, '/sources/control', 'one reviewed audience request covers the complete ordered selection');
    assert.equal(body.source.kind, 'tracks');
    const selected = destinations(body.zone);
    if (body.mode === 'play') {
      return { done: 'Catalog selections playing', changed: true, notes: [], ...(selected.length > 1 ? { batch: { complete: true,
        results: selected.map(zone => ({ household: zone.household, target: player(zone.household, zone.coordinator_room),
          zone: zone.coordinator_room, result: { status: 'succeeded', done: 'Ordered selections playing', changed: true, notes: [] } })) } } : {}) };
    }
    const added = selected.map(zone => {
      const queue = queues.get(Core.key(zone));
      return { household: zone.household, coordinator: player(zone.household, zone.coordinator_room),
        queue: { zone: zone.coordinator_room, done: 'Catalog selections queued', count: body.source.tracks.length,
          first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1,
          playing_queue: queue.playing !== null } };
    });
    return selected.length === 1 ? added[0].queue
      : { done: 'Catalog selections queued', complete: true, added, failed: [], logged: true, undoable: true };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, read, options) : read(url)
      : writer ? writer(url, body, write) : write(url, body);
  };
  const controller = Core.model({ call, changed: () => source?.observe(), now: () => time });
  const doc = dom ? documentFixture() : null, options = { call, controller, completed: value => completed.push(value), now: () => time };
  source = dom ? Dashboard.panel({ ...options, document: doc }) : Dashboard.model(options);
  assert.equal(await controller.refresh(), true); assert.equal(controller.select(origin), true);
  controller.connection(true); source.open(); assert.equal(source.edit('sourceType', 'catalog'), true);
  const search = async (q = 'Calm', kind = 'track') => {
    assert.equal(source.catalog.edit('query', q), true); assert.equal(source.catalog.edit('kind', kind), true);
    assert.equal(await source.catalog.search(), true, source.state.catalog.message);
  };
  const select = index => source.catalog.toggle(source.state.catalog.items[index]);
  return { source, controller, calls, completed, rows, queues, zones, lists, resolution, origin, doc, search, select,
    writes: () => calls.filter(call => call.body !== undefined), read(fn) { reader = fn; }, write(fn) { writer = fn; },
    advance(milliseconds) { time += milliseconds; }, now: () => time,
    changeRevision(value) { revision = value; },
    async compose(target = 'away') {
      await search(); assert.equal(select(0), true);
      assert.equal(await source.catalog.next(), true); assert.equal(select(1), true);
      await search('Talks', 'episode'); assert.equal(select(2), true);
      assert.equal(source.catalog.editSelection('up', 2), true); assert.equal(source.catalog.editSelection('repeat', 0), true);
      if (target !== origin) {
        assert.equal(source.edit('destinationMode', 'set'), true); assert.equal(source.edit('destinationInput', target), true);
      }
      return copy(source.state.catalog.basket.map(row => ({ source_uri: row.item.source_uri, title: row.item.title })));
    },
  };
}

for (const target of ['Den@S2', 'away']) for (const mode of ['play', 'next', 'end']) {
  test(`dashboard catalog ${mode} sends one ordered sequence to ${target}`, async () => {
    const f = await fixture(), tracks = await f.compose(target), before = f.calls.length;
    const reviewed = await f.source.reviewCatalog(mode); assert.ok(reviewed, f.source.state.message);
    assert.deepEqual(reviewed.item.tracks, tracks); assert.equal(reviewed.catalogBasket, f.source.state.catalog.basket);
    assert.equal(reviewed.targets.groups.length, target === f.origin ? 1 : 2);
    assert.equal(f.writes().length, 0); assert.equal(await f.source.apply(reviewed), true);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sources/control', body: {
      zone: target, expected_revision: REVISION, mode: mode === 'end' ? 'append' : mode, source: { kind: 'tracks', tracks },
    } }]);
    assert.equal(f.source.state.result.complete, true); assert.equal(f.source.state.result.title, '4 selected Spotify tracks or episodes');
    assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 1);
    const reads = f.calls.slice(before).filter(call => call.url.startsWith('/spotify/search?'))
      .map(call => Object.fromEntries(new URL(call.url, 'http://localhost').searchParams));
    assert.equal(reads.length, 6, 'each of three selected pages is checked once at review and once before dispatch');
    for (const [q, kind, offset] of [['Calm', 'track', '0'], ['Calm', 'track', '10'], ['Talks', 'episode', '0']]) {
      assert.equal(reads.filter(query => query.q === q && query.kind === kind && String(query.offset || 0) === offset).length, 2);
    }
  });
}

test('search pages and categories retain selections while obsolete result handlers expire', async () => {
  const f = await fixture(); await f.search(); const first = f.source.state.catalog.items;
  assert.equal(f.select(0), true); const retained = f.source.state.catalog.basket;
  assert.equal(await f.source.catalog.next(), true); assert.equal(f.source.state.catalog.basket, retained);
  assert.equal(f.source.catalog.toggle(first[1], first), false); assert.equal(f.select(1), true);
  await f.search('Talks', 'episode'); assert.equal(f.select(2), true);
  assert.deepEqual(f.source.state.catalog.basket.map(row => row.item.source_uri), [uri('track', 1), uri('track', 12), uri('episode', 103)]);
  assert.equal(f.source.state.sourceType, 'catalog'); assert.equal(f.writes().length, 0);
});

test('selection snapshots fence old move, remove and review buttons', async () => {
  const f = await fixture(); await f.search(); f.select(0); const old = f.source.state.catalog.basket;
  f.select(1); const current = f.source.state.catalog.basket;
  assert.equal(f.source.catalog.editSelection('remove', 0, old), false);
  assert.equal(f.source.catalog.editSelection('repeat', 0, old), false);
  assert.equal(await f.source.reviewCatalog('play', old), null); assert.equal(f.source.state.catalog.basket, current);
  assert.equal(f.source.catalog.editSelection('up', 1), true);
  assert.equal(f.source.catalog.editSelection('repeat', 0), true); assert.equal(f.source.catalog.editSelection('remove', 1), true);
  assert.deepEqual(f.source.state.catalog.basket.map(row => row.item.source_uri), [uri('track', 2), uri('track', 1)]);
});

test('repetitions count toward the ten-entry limit and selected rows can still be removed', async () => {
  const f = await fixture(); await f.search(); f.select(0);
  for (let index = 1; index < 10; ++index) assert.equal(f.source.catalog.editSelection('repeat', 0), true);
  const full = f.source.state.catalog.basket;
  assert.equal(f.source.catalog.editSelection('repeat', 0), false); assert.equal(f.select(1), false);
  assert.equal(f.source.state.catalog.basket, full); assert.ok(Object.isFrozen(full));
  assert.equal(f.source.catalog.editSelection('remove', 4), true); assert.equal(f.source.state.catalog.basket.length, 9);
  assert.equal(f.select(0), true); assert.deepEqual(f.source.state.catalog.basket, []);
});

test('album containers keep the existing single-source route and cannot enter a track basket', async () => {
  const f = await fixture(); await f.search('Albums', 'album'); const item = f.source.state.catalog.items[0];
  assert.equal(f.source.catalog.toggle(item), false); assert.equal(f.source.chooseCatalog(item), true);
  assert.equal(f.source.state.sourceType, 'link'); assert.equal(f.source.state.input, item.source_uri);
  assert.deepEqual(f.source.state.catalog.basket, []); assert.equal(f.source.state.pending, null);
});

test('destination and original-group changes keep chosen music but require a fresh review', async () => {
  const f = await fixture(); await f.compose(); const basket = f.source.state.catalog.basket;
  const old = await f.source.reviewCatalog('next'); assert.ok(old);
  f.source.edit('destinationInput', 'everywhere'); assert.equal(f.source.state.pending, null);
  assert.equal(f.source.state.catalog.basket, basket); assert.equal(await f.source.apply(old), false);
  assert.equal(f.controller.select('Office@S2'), true); assert.equal(f.source.state.catalog.basket, basket);
  const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed, f.source.state.message);
  assert.equal(reviewed.selectedTarget, 'Office@S2'); assert.equal(await f.source.apply(reviewed), true);
  assert.equal(f.writes()[0].body.zone, 'everywhere'); assert.equal(f.writes().length, 1);
});

test('query edits cancel playback review without discarding the ordered search selection', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('play'), basket = f.source.state.catalog.basket;
  assert.ok(reviewed); assert.equal(f.source.catalog.edit('query', 'Another search'), true);
  assert.equal(f.source.state.pending, null); assert.equal(f.source.state.catalog.basket, basket);
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 0);
});

for (const change of ['source', 'clear', 'close', 'suspend']) {
  test('catalog selection clears on explicit ' + change, async () => {
    const f = await fixture(); await f.compose();
    if (change === 'source') f.source.edit('sourceType', 'link');
    if (change === 'clear') f.source.clearDraft();
    if (change === 'close') f.source.close();
    if (change === 'suspend') f.source.suspend();
    assert.deepEqual(f.source.state.catalog.basket, []); assert.equal(f.source.state.pending, null); assert.equal(f.writes().length, 0);
  });
}

for (const moment of ['review', 'confirmation']) for (const field of ['title', 'source_uri', 'subtitle']) {
  test(`changed off-page catalog ${field} blocks ${moment} before a write`, async () => {
    const f = await fixture(); await f.compose();
    const reviewed = moment === 'confirmation' ? await f.source.reviewCatalog('next') : null;
    f.lists.get('Calm|track')[0][field] = field === 'source_uri' ? uri('track', 999) : 'Changed catalog metadata';
    if (reviewed) assert.equal(await f.source.apply(reviewed), false);
    else assert.equal(await f.source.reviewCatalog('next'), null);
    assert.equal(f.writes().length, 0); assert.match(f.source.state.message, /changed|again|selection|catalog/i);
  });
}

test('changed search ranking still identifies the exact selected result within its original page', async () => {
  const f = await fixture(); await f.search(); f.select(0);
  const entries = f.lists.get('Calm|track'); [entries[0], entries[4]] = [entries[4], entries[0]];
  const reviewed = await f.source.reviewCatalog('play'); assert.ok(reviewed, f.source.state.message);
  assert.equal(reviewed.item.tracks[0].source_uri, uri('track', 1)); assert.equal(await f.source.apply(reviewed), true);
});

test('a selected result disappearing from its original page requires a new selection', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('play'); assert.ok(reviewed);
  f.lists.get('Talks|episode').splice(2, 1);
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 0);
  assert.equal(f.source.state.result.status, 'not_sent');
});

test('catalog authentication failure retains the basket without sending or retrying a command', async () => {
  const f = await fixture(); await f.compose(); const basket = f.source.state.catalog.basket;
  f.read((url, read) => {
    if (url.startsWith('/spotify/search?')) throw Object.assign(new Error('Spotify sign-in required'), { hint: 'Connect Spotify again.' });
    return read(url);
  });
  assert.equal(await f.source.reviewCatalog('play'), null); assert.equal(f.source.state.catalog.basket, basket);
  assert.match(f.source.state.message, /Connect Spotify/); assert.equal(f.writes().length, 0);
});

test('the original displayed group must still match even when the selection targets other rooms', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed);
  f.zones[0].members.push('New member');
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 0);
  assert.match(f.source.state.message, /original listening group changed/i);
});

test('a changed resolved audience invalidates the complete ordered command', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed);
  f.changeRevision('b'.repeat(64)); assert.equal(await f.source.apply(reviewed), false);
  assert.equal(f.writes().length, 0); assert.match(f.source.state.message, /changed|again/i);
});

test('slow successful Spotify checks leave a full review lifetime and can finish fresh confirmation', async () => {
  const f = await fixture(); await f.compose();
  f.read((url, read) => { const value = read(url); if (url.startsWith('/spotify/search?')) f.advance(50000); return value; });
  const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed, f.source.state.message);
  assert.equal(reviewed.at, f.now()); assert.equal(f.source.canApply(), true);
  f.advance(Dashboard.MAX_AGE - 1); assert.equal(f.source.canApply(), true);
  assert.equal(await f.source.apply(reviewed), true, f.source.state.message);
  assert.equal(f.writes().length, 1); assert.equal(f.source.state.result.complete, true);
});

test('slow catalog confirmation still refuses a changed destination queue', async () => {
  const f = await fixture(); await f.compose();
  f.read((url, read) => { const value = read(url); if (url.startsWith('/spotify/search?')) f.advance(50000); return value; });
  const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed, f.source.state.message);
  f.queues.get('Office@S2').revision = 'queue:changed';
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 0);
  assert.match(f.source.state.message, /queue changed/i);
});

test('an original group that changes during slow Spotify checks is refused before dispatch', async () => {
  const f = await fixture(); await f.compose();
  const reviewed = await f.source.reviewCatalog('next'); assert.ok(reviewed);
  let changed = false;
  f.read((url, read) => {
    const value = read(url);
    if (url.startsWith('/spotify/search?')) {
      f.advance(50000);
      if (!changed) { changed = true; f.zones[0].members.push('New member'); }
    }
    return value;
  });
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 0);
  assert.match(f.source.state.message, /original listening group changed/i);
});

test('a catalog review still expires before admission despite allowing slow provider verification', async () => {
  const f = await fixture(); await f.compose();
  const reviewed = await f.source.reviewCatalog('play'); assert.ok(reviewed);
  f.advance(Dashboard.MAX_AGE); const before = f.calls.length;
  assert.equal(f.source.canApply(), false); assert.equal(await f.source.apply(reviewed), false);
  assert.equal(f.calls.length, before); assert.equal(f.writes().length, 0);
});

for (const change of ['query', 'group', 'close', 'clear']) {
  test('a delayed search cannot restore obsolete page state after ' + change, async () => {
    const f = await fixture(); await f.search(); f.select(0); const basket = f.source.state.catalog.basket, held = deferred(); let waiting = false;
    f.read(async (url, read) => { const value = read(url); if (url.startsWith('/spotify/search?')) { waiting = true; await held.promise; } return value; });
    const task = f.source.catalog.next(); await until(() => waiting);
    if (change === 'query') f.source.catalog.edit('query', 'Changed search');
    if (change === 'group') f.controller.select('Office@S2');
    if (change === 'close') f.source.close();
    if (change === 'clear') f.source.clearDraft();
    held.resolve(); assert.equal(await task, false); assert.deepEqual(f.source.state.catalog.items, []);
    if (change === 'query' || change === 'group') assert.equal(f.source.state.catalog.basket, basket);
    else assert.deepEqual(f.source.state.catalog.basket, []);
    assert.equal(f.writes().length, 0);
  });
}

test('closing during catalog revalidation cancels the pending review before dispatch', async () => {
  const f = await fixture(); await f.compose(); const held = deferred(); let waiting = false;
  f.read(async (url, read) => { const value = read(url); if (url.startsWith('/spotify/search?')) { waiting = true; await held.promise; } return value; });
  const task = f.source.reviewCatalog('play'); await until(() => waiting); f.source.close(); held.resolve();
  assert.equal(await task, null); assert.equal(f.source.state.pending, null); assert.equal(f.writes().length, 0);
});

test('the shared dashboard command lane preserves one dispatch and its receipt through closing', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('next'), held = deferred();
  f.write(async (url, body, write) => { await held.promise; return write(url, body); });
  const task = f.source.apply(reviewed); await until(() => f.writes().length === 1);
  assert.equal(f.controller.state.busy, true); assert.equal(f.source.catalog.editSelection('repeat', 0), false);
  assert.equal(await f.source.apply(reviewed), false); assert.equal(await f.controller.command('resume'), false);
  f.source.close(); held.resolve(); assert.equal(await task, true);
  assert.equal(f.source.state.result.complete, true); assert.equal(f.writes().length, 1);
});

test('ordered catalog playback preserves per-group partial receipts without replay', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('play');
  f.write((url, body, write) => {
    const result = write(url, body); result.batch.complete = false;
    result.batch.results[1].result = { status: 'failed', error: { code: 'OUTCOME_UNCERTAIN', detail: 'The speaker reply was lost', retryable: false } };
    return result;
  });
  assert.equal(await f.source.apply(reviewed), true); assert.equal(f.source.state.result.partial, true);
  assert.equal(f.source.state.result.complete, false); assert.equal(f.source.state.result.uncertain, true);
  assert.match(f.source.state.result.detail, /Office@S2: Confirmed/); assert.match(f.source.state.result.detail, /Bedroom@S1: Outcome uncertain/);
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 1);
});

test('malformed catalog queue receipts remain uncertain and cannot resubmit the old review', async () => {
  const f = await fixture(); await f.compose(); const reviewed = await f.source.reviewCatalog('end');
  f.write((url, body, write) => { const result = write(url, body); result.added[0].queue.count = 1; return result; });
  assert.equal(await f.source.apply(reviewed), false); assert.equal(f.source.state.result.status, 'uncertain');
  assert.equal(f.controller.state.uncertain, true); assert.equal(await f.source.apply(reviewed), false); assert.equal(f.writes().length, 1);
});

test('dashboard DOM exposes exact search provenance, ordered controls and one reviewed confirmation', async () => {
  const f = await fixture({ dom: true }), byId = id => f.doc.getElementById(id);
  await f.search();
  const choose = index => descendants(byId('source-catalog-items').children[index])
    .find(node => node.getAttribute?.('data-catalog-select') != null).fire('click');
  choose(0); const basket = () => descendants(byId('source-online')).find(node => node.getAttribute?.('aria-label') === 'Spotify selection in playback order');
  const action = (name, index) => descendants(basket()).find(node => node.getAttribute?.('data-catalog-basket-action') === name
    && node.getAttribute?.('data-catalog-basket-index') === String(index));
  const oldRemove = action('remove', 0); assert.ok(oldRemove);
  await f.source.catalog.next(); choose(1); oldRemove.fire('click'); assert.equal(f.source.state.catalog.basket.length, 2);
  action('up', 1).fire('click'); action('repeat', 0).fire('click');
  assert.deepEqual(f.source.state.catalog.basket.map(row => row.item.source_uri), [uri('track', 12), uri('track', 12), uri('track', 1)]);
  assert.match(descendants(basket()).map(node => node.textContent).join(' '), /Calm/);
  assert.match(descendants(basket()).map(node => node.textContent).join(' '), /spotify:track:/);
  descendants(byId('source-online')).find(node => node.tag === 'button' && node.textContent === 'Play selected now').fire('click');
  await until(() => !!f.source.state.pending);
  assert.equal(byId('source-preview-selection').hidden, false);
  assert.match(byId('source-preview-selection').children[0].textContent, /Catalog song 12.*spotify:track:.*Calm.*offset 10/);
  assert.match(byId('source-preview-effect').textContent, /Replace each queue/);
  byId('source-confirm').fire('click'); await until(() => f.source.state.result && !f.source.state.writing);
  assert.equal(f.source.state.result.status, 'accepted'); assert.equal(f.writes().length, 1);
});
