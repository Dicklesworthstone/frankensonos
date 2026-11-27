// The shipped phone assembles at most ten exact saved occurrences across
// pages, edits their order, and rechecks only bounded source ranges before send.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const copy = value => structuredClone(value);
const TARGET = 'Listening rooms';
const TARGET_REVISION = 'a'.repeat(64), SOURCE_REVISION = 'b'.repeat(64);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(predicate) {
  for (let i = 0; i < 200 && !predicate(); ++i) await Promise.resolve();
  assert.ok(predicate(), 'reached the asynchronous command boundary');
}
function documentFixture() {
  const doc = { activeElement: null, hidden: false, addEventListener() {}, removeEventListener() {},
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
    },
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
const descendants = node => [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? descendants(child) : [])];

async function fixture({ dom = false, total = 225 } = {}) {
  const member = (household, room, player, coordinator = player) => ({ household, room, player, coordinator });
  const physical = [
    { household: 'S1', coordinator: 'S1_DEN', coordinator_room: 'Den', members: [member('S1', 'Den', 'S1_DEN')] },
    { household: 'S1', coordinator: 'S1_OFFICE', coordinator_room: 'Office',
      members: [member('S1', 'Office', 'S1_OFFICE'), member('S1', 'Study', 'S1_STUDY', 'S1_OFFICE')] },
    { household: 'S1', coordinator: 'S1_KITCHEN', coordinator_room: 'Kitchen', members: [member('S1', 'Kitchen', 'S1_KITCHEN')] },
    { household: 'S2', coordinator: 'S2_DEN', coordinator_room: 'Den', members: [member('S2', 'Den', 'S2_DEN')] },
  ];
  const zones = physical.map(group => ({ household: group.household, coordinator_room: group.coordinator_room,
    members: group.members.map(room => room.room), transport_state: 'paused' }));
  const states = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, {
    zone, transport_state: 'paused', volume: 12, mute: false,
    track: { uri: 'fixture:existing', title: 'Owner music', queue_position: 1, position_secs: 21, duration_secs: 300 },
  }]));
  const queues = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, {
    zone: zone.coordinator_room, revision: 'queue:' + zone.coordinator_room + '@' + zone.household, total: 4,
    playing: 1, items: [{ position: 1, uri: 'fixture:existing', title: 'Owner music' }],
  }]));
  const catalog = [{ id: 'SQ:7', title: 'Saved & repeated' }, { id: 'SQ:8', title: 'Other list' }];
  const entries = Array.from({ length: total }, (_, index) => ({
    title: index === 204 ? '<img src=x onerror=alert(1)> Last selection' : 'Saved track ' + (index + 1),
    uri: 'x-file-cifs://example.invalid/' + (index === 204 ? 2 : index + 1) + '.flac',
  }));
  const h = { physical, zones, states, queues, catalog, entries, calls: [], custom: null, response: null, executions: [],
    revision: 'native:1', sourceRevision: SOURCE_REVISION, targetRevision: TARGET_REVISION };
  h.groupsFor = target => target === TARGET ? [physical[1], physical[2]]
    : target === 'everywhere' ? [physical[1], physical[3]]
      : target === 'all@S1' ? physical.slice(0, 3)
        : physical.filter(group => group.coordinator_room + '@' + group.household === target);
  h.resolution = target => {
    const groups = h.groupsFor(target);
    return { target, revision: target === 'Den@S1' ? h.sourceRevision : h.targetRevision,
      matched: target === TARGET ? 'alias' : target === 'everywhere' || target.startsWith('all@') ? 'everywhere' : 'room',
      rooms: groups.flatMap(group => group.members), groups };
  };
  h.receipt = body => {
    const groups = h.groupsFor(body.target || body.zone), single = groups.length === 1;
    const value = { done: 'Selected saved tracks accepted', changed: true, notes: [],
      zone: single ? groups[0].coordinator_room : body.target, playlist: body.playlist,
      positions: [...body.positions], titles: body.positions.map(position => entries[position - 1].title), mode: body.mode };
    if (body.mode === 'play') {
      if (!single) value.batch = { complete: true, results: groups.map(group => ({
        household: group.household, target: group.coordinator, zone: group.coordinator_room,
        result: { status: 'succeeded', done: 'Selected tracks playing', changed: true },
      })) };
    } else {
      const batch = { done: 'Selected saved tracks queued', complete: true, added: groups.map(group => {
        const queue = queues.get(group.coordinator_room + '@' + group.household);
        return { household: group.household, coordinator: group.coordinator, queue: {
          done: 'Selected tracks queued', zone: group.coordinator_room,
          first: body.mode === 'next' ? queue.playing + 1 : queue.total + 1, count: body.positions.length, playing_queue: true,
        } };
      }), failed: [], logged: true, undoable: true };
      if (body.target) value.queue_batch = batch;
      if (single) Object.assign(value, { first: batch.added[0].queue.first, count: body.positions.length, playing_queue: true });
    }
    return value;
  };
  h.page = query => {
    assert.equal(query.zone, 'Den@S1', 'saved positions are always reread through the original group');
    if (query.expected_revision && query.expected_revision !== h.revision) throw new Error('Saved playlist changed. Read its tracks again.');
    const { offset, limit } = query, items = entries.slice(offset, offset + limit)
      .map((entry, index) => ({ ...entry, position: offset + index + 1 }));
    return { zone: 'Den', playlist: catalog.find(item => item.id === query.playlist),
      revision: h.revision, total: entries.length, offset,
      next_offset: offset + items.length < entries.length ? offset + items.length : null, items };
  };
  h.call = async (name, args = {}) => {
    h.calls.push([name, args]);
    if (h.custom) { const value = h.custom(name, args); if (value !== undefined) return value; }
    let value;
    if (name === 'zones') value = zones;
    else if (name === 'sourceTargetResolve') value = h.resolution(args.query.target);
    else if (name === 'state') value = states.get(args.params.room);
    else if (name === 'sourceQueueState') value = queues.get(args.params.room);
    else if (name === 'sourcePlaylists') { assert.equal(args.query.zone, 'Den@S1'); value = catalog; }
    else if (name === 'sourcePlaylistContents') value = h.page(args.query);
    else if (name === 'sourcePlaylistTracks') value = h.response || h.receipt(args.body);
    else throw new Error('Unexpected route ' + name);
    return copy(value);
  };
  const view = dom ? documentFixture() : null;
  const options = { call: h.call, execute: async (group, work) => { h.executions.push({ kind: 'group', groups: [group] }); return work(); },
    executeSet: async (groups, work) => { h.executions.push({ kind: 'set', groups }); return work(); } };
  h.api = dom ? P.panel({ ...options, el: view.el, document: view.doc }) : P.model(options); h.doc = view?.doc;
  assert.equal(await h.api.refresh(), true); assert.equal(h.api.edit('target', 'Den@S1'), true);
  assert.equal(h.api.edit('sourceType', 'playlist'), true); assert.equal(await h.api.loadPlaylists(), true);
  assert.equal(h.api.edit('playlist', 'SQ:7'), true); assert.equal(await h.api.loadContents(), true);
  h.writes = () => h.calls.filter(([, args]) => args.body !== undefined);
  return h;
}

async function chooseAcrossPages(h) {
  assert.equal(h.api.selectTrack(2), true); assert.equal(h.api.selectTrack(100), true);
  const first = h.api.state.contentsBasket;
  assert.equal(await h.api.loadContents('next'), true, h.api.state.message);
  assert.equal(h.api.state.contentsBasket, first, 'a page change retains the actual bounded basket');
  assert.equal(h.api.selectTrack(101), true);
  assert.equal(await h.api.loadContents('next'), true, h.api.state.message);
  assert.equal(h.api.selectTrack(205), true);
  assert.deepEqual(h.api.state.contentsSelection, [2, 100, 101, 205]);
  for (const index of [3, 2, 1]) assert.equal(h.api.changeTrackSelection('up', index), true);
  assert.equal(h.api.changeTrackSelection('repeat', 0), true);
  assert.equal(h.api.changeTrackSelection('down', 1), true);
  assert.deepEqual(h.api.state.contentsSelection, [205, 2, 205, 100, 101]);
}

for (const mode of ['play', 'next', 'end']) for (const target of ['', TARGET]) {
  test('phone playlist basket: ' + mode + ' retains cross-page order and repeats for ' + (target || 'the source group'), async () => {
    const h = await fixture(); await chooseAcrossPages(h);
    const basket = h.api.state.contentsBasket;
    assert.ok(Object.isFrozen(basket)); assert.ok(Object.isFrozen(basket.tracks)); assert.equal(basket.tracks.length, 5);
    assert.equal(basket.group.target, 'Den@S1'); assert.equal(basket.revision, 'native:1'); assert.equal(basket.total, 225);
    h.api.edit('selectionTarget', target); const readsBefore = h.calls.length;
    assert.equal(await h.api.reviewTracks(mode), true, h.api.state.message); const p = h.api.state.pending;
    assert.deepEqual(p.item.positions, [205, 2, 205, 100, 101]);
    assert.equal(p.item.tracks[0].uri, p.item.tracks[1].uri, 'different saved occurrences can contain the same resource');
    assert.equal(p.item.tracks[0].uri, p.item.tracks[2].uri, 'an explicit repeat preserves the selected saved occurrence');
    assert.equal(p.body.zone, 'Den@S1'); assert.equal(p.body.expected_revision, 'native:1');
    assert.equal(p.body.target, target || undefined); assert.equal(h.writes().length, 0);
    assert.equal(await h.api.confirm(p), true, h.api.state.result?.detail); assert.equal(await h.api.confirm(p), false);
    assert.equal(h.writes().length, 1); assert.equal(h.writes()[0][0], 'sourcePlaylistTracks');
    assert.deepEqual(h.writes()[0][1].body.positions, [205, 2, 205, 100, 101]);
    assert.equal(h.writes()[0][1].body.mode, mode === 'end' ? 'append' : mode);
    assert.equal(h.executions[0].kind, target ? 'set' : 'group');
    const ranges = h.calls.slice(readsBefore).filter(([name]) => name === 'sourcePlaylistContents').map(([, args]) => args.query);
    assert.deepEqual(ranges.map(query => [query.offset, query.limit]), [[1, 100], [204, 1], [1, 100], [204, 1]]);
    for (const query of ranges) {
      assert.equal(query.zone, 'Den@S1'); assert.equal(query.playlist, 'SQ:7');
      assert.equal(query.expected_revision, 'native:1'); assert.ok(query.limit <= 100);
    }
    assert.equal(h.api.state.contentsBasket, basket, 'dispatch does not rewrite the local selection or save the playlist');
  });
}

test('phone playlist basket: navigation remembers only selected rows and returns to the same source occurrence', async () => {
  const h = await fixture(); h.api.selectTrack(2); const firstPage = h.api.state.contents, firstBasket = h.api.state.contentsBasket;
  assert.equal(await h.api.loadContents('next'), true);
  assert.equal(h.api.state.contentsBasket, firstBasket); assert.deepEqual(h.api.state.contentsSelection, [2]);
  assert.equal(h.api.selectTrack(2, firstPage), false, 'an old page button cannot act on a later page');
  assert.equal(h.api.selectTrack(101), true); const basket = h.api.state.contentsBasket;
  assert.equal(await h.api.loadContents('previous'), true);
  assert.equal(h.api.state.contentsBasket, basket); assert.deepEqual(h.api.state.contentsSelection, [2, 101]);
  assert.equal(h.api.changeTrackSelection('repeat', 0), true); assert.deepEqual(h.api.state.contentsSelection, [2, 2, 101]);
  assert.equal(h.api.selectTrack(2), true, 'unselecting a visible source occurrence removes its selected repeats');
  assert.deepEqual(h.api.state.contentsSelection, [101]); assert.equal(h.api.state.contentsBasket.tracks.length, 1);
  assert.equal(Object.hasOwn(h.api.state.contentsBasket, 'pages'), false);
  assert.equal(h.writes().length, 0);
});

test('phone playlist basket: edits retire consent and stale basket handlers cannot alter the current order', async () => {
  const h = await fixture(); await chooseAcrossPages(h); assert.equal(await h.api.reviewTracks('play'), true);
  const p = h.api.state.pending, basket = h.api.state.contentsBasket, page = h.api.state.contents;
  assert.equal(h.api.changeTrackSelection('remove', 0, basket), true); const current = h.api.state.contentsBasket;
  assert.equal(h.api.state.pending, null); assert.equal(h.api.state.contents, page);
  assert.deepEqual(h.api.state.contentsSelection, [2, 205, 100, 101]);
  assert.equal(h.api.changeTrackSelection('repeat', 1, basket), false);
  assert.equal(h.api.changeTrackSelection('down', 0, basket), false);
  assert.equal(await h.api.reviewTracks('next', page, basket), false);
  assert.equal(h.api.state.contentsBasket, current);
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
  assert.equal(await h.api.review(), true); assert.deepEqual(h.api.state.pending.item.positions, [2, 205, 100, 101]);
});

test('phone playlist basket: ten occurrences is a hard bound across repeated items and pages', async () => {
  const h = await fixture(); h.api.selectTrack(2);
  for (let i = 1; i < 10; i++) assert.equal(h.api.changeTrackSelection('repeat', 0), true);
  const full = h.api.state.contentsBasket;
  assert.equal(full.tracks.length, 10); assert.equal(h.api.changeTrackSelection('repeat', 0), false);
  assert.equal(h.api.state.contentsBasket, full);
  assert.equal(await h.api.loadContents('next'), true); assert.equal(h.api.selectTrack(101), false);
  assert.equal(h.api.state.contentsBasket, full);
  assert.equal(h.api.changeTrackSelection('remove', 4), true); assert.equal(h.api.selectTrack(101), true);
  assert.equal(h.api.state.contentsBasket.tracks.length, 10);
  assert.deepEqual(h.api.state.contentsSelection, [...Array(9).fill(2), 101]);
  assert.equal(h.api.changeTrackSelection('up', 0), false); assert.equal(h.api.changeTrackSelection('down', 9), false);
  assert.equal(h.api.changeTrackSelection('remove', -1), false); assert.equal(h.api.changeTrackSelection('remove', 10), false);
  assert.equal(h.api.changeTrackSelection('remove', 1.2), false);
  assert.equal(h.writes().length, 0);
});

test('phone playlist basket: explicit refresh and source boundaries clear retained off-page choices', async () => {
  for (const action of ['refresh', 'group', 'source', 'playlist', 'catalog', 'clear']) {
    const h = await fixture(); await chooseAcrossPages(h);
    const old = h.api.state.contentsBasket;
    if (action === 'refresh') assert.equal(await h.api.loadContents('refresh'), true);
    if (action === 'group') h.api.edit('target', 'Den@S2');
    if (action === 'source') h.api.edit('sourceType', 'link');
    if (action === 'playlist') h.api.edit('playlist', 'SQ:8');
    if (action === 'catalog') h.api.catalogChanged({ household: 'S1', playlists: true });
    if (action === 'clear') h.api.clearTrackSelection();
    assert.equal(h.api.state.contentsBasket, null, action); assert.deepEqual(h.api.state.contentsSelection, [], action);
    assert.equal(h.api.changeTrackSelection('repeat', 0, old), false); assert.equal(h.writes().length, 0);
  }
});

test('phone playlist basket: failed page fetch retains the previous view and picks for a deliberate retry', async () => {
  const h = await fixture(); h.api.selectTrack(2); const page = h.api.state.contents, basket = h.api.state.contentsBasket;
  h.custom = name => { if (name === 'sourcePlaylistContents') throw new Error('Speaker temporarily unavailable'); };
  assert.equal(await h.api.loadContents('next'), false);
  assert.equal(h.api.state.contents, page); assert.equal(h.api.state.contentsBasket, basket);
  assert.deepEqual(h.api.state.contentsSelection, [2]); assert.deepEqual(h.api.state.contentsHistory, []);
  assert.match(h.api.state.message, /temporarily unavailable/);
  h.custom = null; assert.equal(await h.api.loadContents('next'), true);
  assert.equal(h.api.state.contents.offset, 100); assert.equal(h.api.state.contentsBasket, basket);
  assert.equal(h.writes().length, 0);
});

test('phone playlist basket: contradictory navigation replies retire retained choices', async () => {
  for (const change of ['revision', 'title', 'total', 'selected URI', 'selected title']) {
    const h = await fixture(); h.api.selectTrack(2); await h.api.loadContents('next');
    h.custom = (name, args) => {
      if (name !== 'sourcePlaylistContents') return;
      const page = copy(h.page(args.query));
      if (change === 'revision') page.revision = 'other revision';
      if (change === 'title') page.playlist.title = 'Another playlist';
      if (change === 'total') page.total++;
      if (change === 'selected URI') page.items[1].uri = 'fixture:substitution';
      if (change === 'selected title') page.items[1].title = 'Changed selected occurrence';
      return page;
    };
    assert.equal(await h.api.loadContents('previous'), false, change);
    assert.equal(h.api.state.contentsBasket, null, change); assert.deepEqual(h.api.state.contentsSelection, [], change);
    assert.equal(h.api.state.contents, null, change); assert.equal(h.writes().length, 0);
  }
});

test('phone playlist basket: a daemon revision rejection clears the old selection before retry', async () => {
  const h = await fixture(); h.api.selectTrack(2); const old = h.api.state.contentsBasket;
  h.custom = name => {
    if (name === 'sourcePlaylistContents') throw Object.assign(new Error('Saved playlist changed'), { code: 'QUEUE_CHANGED' });
  };
  assert.equal(await h.api.loadContents('next'), false); assert.equal(h.api.state.contentsBasket, null);
  assert.equal(h.api.state.contents, null); assert.deepEqual(h.api.state.contentsSelection, []);
  assert.equal(h.api.changeTrackSelection('repeat', 0, old), false); assert.equal(h.writes().length, 0);
});

test('phone playlist basket: a changed later selected range prevents the entire operation', async () => {
  for (const target of ['', TARGET]) for (const phase of ['review', 'confirm']) {
    const h = await fixture(); await chooseAcrossPages(h); h.api.edit('selectionTarget', target);
    let p;
    if (phase === 'confirm') { assert.equal(await h.api.reviewTracks('next'), true); p = h.api.state.pending; }
    h.entries[204].uri = 'fixture:changed-later-selection';
    const before = h.calls.length;
    if (phase === 'review') assert.equal(await h.api.reviewTracks('next'), false);
    else assert.equal(await h.api.confirm(p), false);
    assert.equal(h.writes().length, 0); assert.equal(h.api.state.pending, null);
    const ranges = h.calls.slice(before).filter(([name]) => name === 'sourcePlaylistContents');
    assert.equal(ranges.length, 2); assert.equal(ranges[1][1].query.offset, 204);
  }
});

test('phone playlist basket: invalidation between range reads cannot continue or resurrect consent', async () => {
  for (const target of ['', TARGET]) {
    const h = await fixture(); await chooseAcrossPages(h); h.api.edit('selectionTarget', target);
    const gate = deferred(); let waiting = false, count = 0;
    h.custom = (name, args) => {
      if (name === 'sourcePlaylistContents') {
        count++; waiting = true; const page = copy(h.page(args.query));
        return gate.promise.then(() => page);
      }
    };
    const task = h.api.reviewTracks('play'); await until(() => waiting);
    h.api.catalogChanged({ household: 'S1', playlists: true }); gate.resolve();
    assert.equal(await task, false); assert.equal(count, 1, 'the next selected range is never requested');
    assert.equal(h.api.state.contentsBasket, null); assert.equal(h.api.state.pending, null); assert.equal(h.writes().length, 0);
  }
});

test('phone playlist basket: a submitted reordered selection is not replayed after a lost response', async () => {
  const h = await fixture(); await chooseAcrossPages(h); h.api.edit('selectionTarget', TARGET);
  assert.equal(await h.api.reviewTracks('end'), true); const p = h.api.state.pending;
  h.custom = name => { if (name === 'sourcePlaylistTracks') throw new Error('Reply lost after the request'); };
  assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain');
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 1);
  assert.deepEqual(h.writes()[0][1].body.positions, [205, 2, 205, 100, 101]);
});

test('phone playlist basket DOM: off-page rows expose ordered editing with stale-handler and in-flight guards', async () => {
  const h = await fixture({ dom: true }), all = () => descendants(h.api.node);
  const list = () => all().find(node => node.getAttribute?.('aria-label') === 'Selected tracks in playback order');
  const button = (action, index) => descendants(list()).find(node => node.getAttribute?.('data-playlist-basket-action') === action
    && node.getAttribute?.('data-playlist-basket-index') === String(index));
  h.api.selectTrack(2); await h.api.loadContents('next'); h.api.selectTrack(101);
  assert.deepEqual(list().children.map(node => node.textContent), ['Track 2: Saved track 2', 'Track 101: Saved track 101']);
  const stale = button('repeat', 0); stale.focus(); stale.fire('click');
  assert.deepEqual(h.api.state.contentsSelection, [2, 2, 101]);
  assert.equal(h.doc.activeElement.getAttribute('data-playlist-basket-action'), 'repeat');
  const snapshot = h.api.state.contentsBasket; stale.fire('click'); assert.equal(h.api.state.contentsBasket, snapshot);
  button('down', 1).focus(); button('down', 1).fire('click'); assert.deepEqual(h.api.state.contentsSelection, [2, 101, 2]);
  assert.ok(list().contains(h.doc.activeElement), 'moving to a boundary keeps keyboard focus in the selected row');
  assert.equal(h.doc.activeElement.getAttribute('data-playlist-basket-index'), '2');
  button('remove', 0).fire('click'); assert.deepEqual(h.api.state.contentsSelection, [101, 2]);
  assert.equal(button('up', 0).disabled, true); assert.equal(button('down', 1).disabled, true);
  await h.api.loadContents('next'); h.api.selectTrack(205);
  assert.equal(all().some(node => node.tag === 'img'), false);
  assert.ok(list().children.some(node => node.textContent === 'Track 205: <img src=x onerror=alert(1)> Last selection'));
  while (h.api.state.contentsSelection.length < 10) h.api.changeTrackSelection('repeat', 0);
  assert.ok(descendants(list()).filter(node => node.getAttribute?.('data-playlist-basket-action') === 'repeat').every(node => node.disabled));
  assert.equal(await h.api.reviewTracks('next'), true); const p = h.api.state.pending;
  const reviewed = all().find(node => node.getAttribute?.('aria-label') === 'Reviewed saved tracks in playback order');
  assert.deepEqual(reviewed.children.map(node => node.textContent), list().children.map(node => node.textContent));
  const gate = deferred(); h.custom = (name, args) => name === 'sourcePlaylistTracks' ? gate.promise.then(() => h.receipt(args.body)) : undefined;
  const task = h.api.confirm(p); await until(() => h.writes().length === 1);
  assert.ok(descendants(list()).filter(node => node.getAttribute?.('data-playlist-basket-action')).every(node => node.disabled));
  assert.equal(h.api.changeTrackSelection('remove', 0), false); assert.equal(await h.api.loadContents('previous'), false);
  gate.resolve(); assert.equal(await task, true); assert.equal(h.writes().length, 1);
});

test('phone playlist basket DOM: removing the last retained row restores whole-playlist review', async () => {
  const h = await fixture({ dom: true }); h.api.selectTrack(2); await h.api.loadContents('next');
  const row = descendants(h.api.node).find(node => node.getAttribute?.('data-playlist-basket-action') === 'remove');
  row.fire('click'); assert.equal(h.api.state.contentsBasket, null); assert.deepEqual(h.api.state.contentsSelection, []);
  assert.equal(await h.api.review(), true); assert.equal(h.api.state.pending.item.kind, 'sonos_playlist');
  assert.equal(h.writes().length, 0);
});
