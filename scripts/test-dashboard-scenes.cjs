'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Scenes = require('../crates/fsonos-api/src/remote/dashboard-scenes.js');
const LibraryPanel = require('../crates/fsonos-api/src/remote/dashboard-library-panel.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 200 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, tagName: tag.toUpperCase(), children: [], dataset: {}, isConnected: true,
      open: false, hidden: false, disabled: false, checked: false, textContent: '', value: '',
      attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
      getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      removeEventListener(name, listener) { if (listeners.get(name) === listener) listeners.delete(name); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      descendants() { return descendants(this); }, contains(other) { return this === other || descendants(this).includes(other); },
      closest() { return null; }, before() {}, remove() { this.isConnected = false; },
      querySelectorAll(selector) { return descendants(this).filter(node => selector === '[data-catalog-key]'
        ? node.dataset.catalogKey !== undefined : node.tag === selector); },
      querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; } };
  }, createTextNode(text) { return String(text); }, getElementById(id) {
    if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id });
    return nodes.get(id);
  } };
  return doc;
}
function fixture({ demo = false, dom = false, network } = {}) {
  const zones = ['S2', 'S1'].map((household, i) => ({ household, coordinator_room: 'Main room',
    members: i ? ['Main room'] : ['Main room', 'Side room'], transport_state: i ? 'paused' : 'playing' }));
  const rows = new Map(zones.map(zone => [C.key(zone), { zone, transport_state: zone.transport_state, volume: 25, mute: false,
    track: { uri: 'fixture:track', title: 'Synthetic music', queue_position: 2, position_secs: 30, duration_secs: 180 } }]));
  const capture = (name, backupQueues) => ({ name, saved: 1700000000, revision: 'scene-v1:fixture-' + name, groups: zones.map(zone => ({
    coordinator: C.key(zone), members: zone.members.filter(room => room !== zone.coordinator_room).map(room => room + '@' + zone.household),
    playing: rows.get(C.key(zone)).transport_state === 'playing', source: { kind: backupQueues ? 'queue_backup' : 'queue',
      queue: { coordinator: 'fixture-player-' + zone.household, household: zone.household, update_id: 9,
        track: 2, position_secs: 30, play_mode: 'NORMAL', ...(backupQueues ? { total: 12 } : {}) } },
  })), volumes: { 'Main room@S2': 25, 'Side room@S2': 15, 'Main room@S1': 20 }, mutes: { 'Side room@S2': false } });
  const catalog = [capture('Dinner', false)], calls = [], saved = [], recalled = [];
  let time = 1000, reader = null, writer = null, capturer, onSaved = null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url === '/scenes') return copy(catalog);
    if (url.startsWith('/rooms/resolve?')) {
      const target = new URL(url, 'http://fixture').searchParams.get('target');
      const match = /^(.*)@(S[12]|SIM_S[12])$/.exec(target);
      if (!match) throw new Error('Unknown fixture room');
      return { target, matched: 'room', rooms: [{ room: match[1], household: match[2].replace('SIM_', '') }] };
    }
    if (url.startsWith('/zones/') && url.endsWith('/state')) return copy(rows.get(decodeURIComponent(url.slice(7, -6))));
    throw new Error('Unexpected fixture read: ' + url);
  };
  const normalWrite = (url, body, options) => {
    assert.equal(options?.method, 'PUT'); assert.match(url, /^\/scenes\/[^/]+$/);
    const name = decodeURIComponent(url.slice(8));
    const previous = catalog.find(scene => scene.name.toLowerCase() === name.toLowerCase());
    assert.equal(body.expected_revision, previous?.revision || 'absent');
    const { expected_revision, ...definition } = body;
    const result = body.groups ? { name, saved: 1700000000, revision: 'scene-v1:fixture-' + name, ...copy(definition) } : capture(name, body.backup_queues === true);
    const index = catalog.findIndex(scene => scene.name === result.name);
    if (index < 0) catalog.push(result); else catalog[index] = result;
    return copy(result);
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, options, normalWrite) : normalWrite(url, body, options);
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const call = network ? network(raw) : raw;
  const ctl = C.model({ call, demo, now: () => time, changed: () => capturer?.observe() });
  const session = { state: { scenes: [] }, async catalog() {
    if (onSaved) await onSaved();
    session.state.scenes = Scenes.catalogOf(await call('/scenes')); saved.push(session.state.scenes);
  }, reviewScene(name) { recalled.push(name); } };
  const options = { call, controller: ctl, now: () => time, saved: () => session.catalog() };
  const doc = dom ? documentFixture() : null;
  capturer = dom ? Scenes.panel({ ...options, session, document: doc }) : Scenes.model(options);
  return { ctl, capturer, zones, rows, catalog, calls, call, saved, recalled, doc, capture, normalRead, normalWrite, session,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body, options }) => ({ url, body, method: options?.method })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; }, afterSave(fn) { onSaved = fn; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}
async function review(f, name = 'Quiet / evening', backup = false) {
  assert.equal(await f.capturer.open(), true); f.capturer.edit('name', name); f.capturer.edit('backupQueues', backup);
  const pending = f.capturer.review(); assert.ok(pending); return pending;
}

const selectedMusic = () => ({ kind: 'tracks', group: { target: 'Main room@S2', members: ['Main room@S2', 'Side room@S2'] },
  tracks: [{ source_uri: `spotify:track:${'A'.repeat(22)}`, title: 'First <img src=x>' },
    { source_uri: `spotify:episode:${'B'.repeat(22)}`, title: 'Second' },
    { source_uri: `spotify:track:${'A'.repeat(22)}`, title: 'Repeated' }] });

test('a library selection opens a named scene draft and saves only the reviewed ordered group', async () => {
  for (const playing of [true, false]) {
    const f = await fixture().ready(), start = f.calls.length;
    assert.equal(await f.capturer.openSelection(selectedMusic()), true);
    assert.equal(f.capturer.state.name, ''); assert.equal(f.capturer.canReview(), false);
    assert.equal(f.capturer.edit('name', 'Selected / music'), true);
    assert.equal(f.capturer.edit('playing', playing), true);
    assert.equal(f.capturer.edit('backupQueues', true), false);
    const pending = f.capturer.review(); assert.ok(pending);
    assert.deepEqual(pending.body, { groups: [{ coordinator: 'Main room@S2', members: ['Side room@S2'],
      source: { kind: 'tracks', tracks: selectedMusic().tracks }, playing }], volumes: {}, mutes: {} });
    assert.deepEqual(f.writes(), []);
    assert.equal(await f.capturer.apply(pending), true, f.capturer.state.message);
    assert.equal(await f.capturer.apply(pending), false);
    assert.deepEqual(f.writes(), [{ url: '/scenes/Selected%20%2F%20music', body: { ...pending.body, expected_revision: 'absent' }, method: 'PUT' }]);
    const sent = f.calls.findIndex((call, index) => index >= start && call.body !== undefined);
    assert.ok(f.calls.slice(start, sent).every(call => ['/zones', '/scenes'].includes(call.url)),
      'reviewing a concrete selection needs no current music, queue or DJ state');
    assert.ok(f.calls.slice(start).every(call => !/\/queue|\/dj/.test(call.url)));
    assert.equal(f.capturer.state.result.groups[0].playing, playing);
    assert.match(f.capturer.state.message, /Playback was not changed/);
  }
});

test('selection import rejects unfinished editors, missing groups, malformed tracks and demo writes', async () => {
  for (const change of ['open', 'target', 'members', 'bound', 'demo']) {
    const f = await fixture({ demo: change === 'demo' }).ready(), selection = selectedMusic();
    if (change === 'open') await f.capturer.open();
    if (change === 'target') selection.group.target = 'Elsewhere@S2';
    if (change === 'members') selection.group.members.pop();
    if (change === 'bound') selection.tracks = Array(11).fill(selection.tracks[0]);
    assert.equal(await f.capturer.openSelection(selection), false, change);
    assert.deepEqual(f.writes(), []);
  }
});

test('library scene group changes, replacement conflicts and edited playback consume previous reviews', async () => {
  for (const change of ['groups', 'selection', 'playback', 'occupied']) {
    const f = await fixture().ready(); assert.equal(await f.capturer.openSelection(selectedMusic()), true);
    f.capturer.edit('name', 'Library scene'); const pending = f.capturer.review();
    if (change === 'groups') f.zones[0].members.pop();
    if (change === 'selection') f.ctl.select('Main room@S1');
    if (change === 'playback') f.capturer.edit('playing', false);
    if (change === 'occupied') f.catalog.push(f.capture('Library scene', false));
    assert.equal(await f.capturer.apply(pending), false, change);
    assert.deepEqual(f.writes(), []);
    if (change === 'selection') {
      assert.equal(await f.capturer.read(), false);
      assert.equal(f.capturer.canReview(), false);
    }
  }
});

test('canonical room spellings confirm the selected group but changed saved music never claims success', async () => {
  for (const change of ['canonical', 'room', 'order', 'playing', 'volume']) {
    const f = await fixture().ready(); assert.equal(await f.capturer.openSelection(selectedMusic()), true);
    f.capturer.edit('name', 'Library scene'); const pending = f.capturer.review();
    f.write((url, body, options, normal) => {
      const saved = normal(url, body, options), group = saved.groups[0];
      group.coordinator = group.coordinator.replace('@S2', '@SIM_S2');
      group.members = group.members.map(room => room.replace('@S2', '@SIM_S2'));
      if (change === 'room') group.members = ['Other room@SIM_S2'];
      if (change === 'order') group.source.tracks.reverse();
      if (change === 'playing') group.playing = false;
      if (change === 'volume') saved.volumes = { 'Main room@S2': 100 };
      return saved;
    });
    assert.equal(await f.capturer.apply(pending), change === 'canonical', f.capturer.state.message);
    assert.equal(await f.capturer.apply(pending), false); assert.equal(f.writes().length, 1);
    if (change !== 'canonical') { assert.equal(f.capturer.state.result, null); assert.equal(f.ctl.state.uncertain, true); }
  }
});

test('actual dashboard selection scene controls expose order, paused recall and reviewed storage', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  assert.equal(await f.capturer.openSelection(selectedMusic()), true);
  assert.equal($('capture-scene-dialog').open, true);
  assert.equal($('capture-scene-backup-option').hidden, true);
  assert.equal($('capture-scene-playback-option').hidden, false);
  assert.match($('capture-scene-tracks').textContent, /1\. First <img src=x>[\s\S]*2\. Second[\s\S]*3\. Repeated/);
  $('capture-scene-name').value = 'My selection'; $('capture-scene-name').fire('input');
  $('capture-scene-playback').value = 'pause'; $('capture-scene-playback').fire('change');
  $('capture-scene-review').fire('click');
  assert.match($('capture-scene-preview-detail').textContent, /without starting playback/);
  assert.match($('capture-scene-review-note').textContent, /Apply the scene separately or choose it in Routines/);
  assert.equal($('capture-scene-playback').disabled, true); assert.deepEqual(f.writes(), []);
  $('capture-scene-confirm').fire('click'); await settled(() => !f.capturer.state.writing);
  assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].body.groups[0].playing, false);
  assert.equal(f.capturer.state.result.name, 'My selection');
});

test('actual dashboard basket button rechecks music and hands repeated tracks to the scene save dialog', async t => {
  for (const changed of [false, true]) {
    const f = await fixture({ dom: true }).ready();
    let revision = 'observed-library';
    f.read((url, options, normal) => {
      if (!url.startsWith('/library/browse?')) return normal(url);
      const query = new URL(url, 'http://fixture').searchParams, kind = query.get('kind');
      return { kind, offset: 0, limit: 50, album_uri: null, total: 2, next_offset: null, coverage: 'cached', revision,
        items: ['A', 'B'].map(letter => ({ kind: kind === 'tracks' ? 'track' : 'album',
          source_uri: `spotify:${kind === 'tracks' ? 'track' : 'album'}:${letter.repeat(22)}`,
          title: `Selected ${letter}`, subtitle: 'Fixture artist', explicit: false, ...(kind === 'albums' ? { cached_tracks: 2 } : {}) })) };
    });
    let offered = 0;
    const panel = LibraryPanel.create({ call: f.call, controller: f.ctl, session: f.session, doc: f.doc,
      offerScene(value) {
        offered++; if (f.capturer.state.open) return false;
        void f.capturer.openSelection(value); return f.capturer.state.open;
      } });
    t.after(() => panel.dispose());
    f.doc.getElementById('search-dialog').showModal(); panel.node.open = true;
    await panel.model.open(); await panel.model.browse('tracks');
    const buttons = () => panel.node.descendants().filter(node => node.tag === 'button');
    const checkbox = panel.node.descendants().find(node => node.dataset.catalogKey === 'select:0');
    checkbox.checked = true; checkbox.fire('change');
    buttons().find(node => node.dataset.catalogKey?.endsWith(':Repeat track')).fire('click');
    assert.equal(panel.model.state.basket.length, 2);
    const save = buttons().find(node => node.textContent === 'Save selection as scene');
    assert.equal(save.disabled, false);
    if (changed) revision = 'changed-library';
    save.fire('click'); await new Promise(setImmediate);
    assert.equal(offered, changed ? 0 : 1);
    assert.deepEqual(f.writes(), []);
    if (changed) {
      assert.equal(f.doc.getElementById('search-dialog').open, true);
      assert.equal(panel.model.state.basket.length, 2);
      assert.equal(f.capturer.state.open, false);
      assert.ok(panel.node.descendants().some(node => node.attributes.role === 'status' && /changed/i.test(node.textContent)));
    } else {
      assert.equal(f.doc.getElementById('search-dialog').open, false);
      assert.equal(f.doc.getElementById('capture-scene-dialog').open, true);
      assert.equal(f.capturer.state.selection.tracks.length, 2);
      assert.deepEqual(f.capturer.state.selection.tracks[0], f.capturer.state.selection.tracks[1]);
      assert.deepEqual(f.capturer.state.selection.group.members, selectedMusic().group.members);
    }
  }
});

test('capture is lazy and reads the whole-house inventory and existing definitions before review', async () => {
  const f = await fixture().ready(); assert.equal(f.calls.some(row => row.url === '/scenes'), false);
  const before = f.calls.length; assert.equal(await f.capturer.open(), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones', '/scenes']);
  assert.deepEqual(f.capturer.state.groups.map(C.key), ['Main room@S2', 'Main room@S1']);
  assert.equal(f.capturer.state.scenes[0].name, 'Dinner'); assert.equal(f.writes().length, 0);
});

test('native scene names trim whitespace and count Unicode code points up to 64', () => {
  for (const value of ['', '  ', null, 42, 'a'.repeat(65), '🎵'.repeat(65)]) assert.equal(Scenes.nameOf(value), null);
  assert.equal(Scenes.nameOf('  Quiet / evening  '), 'Quiet / evening'); assert.equal(Scenes.nameOf('🎵'.repeat(64)), '🎵'.repeat(64));
});

test('capture uses one native PUT, saves the whole house, then refreshes the catalog without altering playback', async () => {
  const f = await fixture().ready(), beforeState = copy([...f.rows]);
  const p = await review(f); assert.ok(Object.isFrozen(p)); assert.ok(Object.isFrozen(p.body)); assert.ok(Object.isFrozen(p.groups));
  const before = f.calls.length;
  assert.equal(await f.capturer.apply(p), true);
  assert.deepEqual(f.writes(), [{ url: '/scenes/Quiet%20%2F%20evening', body: { expected_revision: 'absent' }, method: 'PUT' }]);
  assert.deepEqual([...f.rows], beforeState); assert.equal(f.capturer.state.result.groups.length, 2);
  assert.equal(f.capturer.state.result.groups[0].source.kind, 'queue');
  assert.equal(f.capturer.state.result.groups[1].playing, false); assert.equal(f.saved.length, 1);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones', '/scenes', '/scenes/Quiet%20%2F%20evening',
    '/zones', '/zones/Main%20room%40S2/state', '/zones/Main%20room%40S1/state', '/scenes']);
  assert.equal(await f.capturer.apply(p), false); assert.match(f.ctl.state.message, /Playback was not changed/);
});

test('complete queue backups use the exact native capture option and retain restore metadata', async () => {
  const f = await fixture().ready(); await review(f, 'Queue for later', true);
  assert.equal(await f.capturer.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/scenes/Queue%20for%20later', body: { backup_queues: true, expected_revision: 'absent' }, method: 'PUT' }]);
  const source = f.capturer.state.result.groups[0].source;
  assert.equal(source.kind, 'queue_backup'); assert.equal(source.queue.total, 12); assert.equal(source.queue.track, 2);
  assert.equal(source.queue.position_secs, 30); assert.equal(source.queue.play_mode, 'NORMAL');
});

test('replacement review retains the exact existing definition and canonical stored spelling', async () => {
  const f = await fixture().ready(), p = await review(f, '  dINNeR  ', true);
  assert.equal(p.name, 'Dinner'); assert.equal(p.existing.name, 'Dinner'); assert.ok(Object.isFrozen(p.existing));
  assert.equal(p.existing.groups[0].source.kind, 'queue');
  assert.equal(await f.capturer.apply(), true); assert.equal(f.catalog.length, 1);
  assert.deepEqual(f.writes(), [{ url: '/scenes/Dinner', body: { backup_queues: true, expected_revision: p.existing.revision }, method: 'PUT' }]);
});

test('case-ambiguous saved names fail closed without creating another saved definition', async () => {
  const f = await fixture().ready(); f.catalog.push(f.capture('dinner', false));
  assert.equal(await f.capturer.open(), false); assert.match(f.capturer.state.message, /ambiguous/);
  f.capturer.edit('name', 'New name'); assert.equal(f.capturer.review(), null); assert.equal(f.writes().length, 0);
});

test('newly occupied names, changed definitions and removed replacements reject preflight', async () => {
  for (const kind of ['occupied', 'changed', 'removed']) {
    const f = await fixture().ready(); await review(f, kind === 'occupied' ? 'New scene' : 'Dinner');
    if (kind === 'occupied') f.catalog.push(f.capture('new scene', false));
    else if (kind === 'changed') f.catalog[0].volumes['Side room@S2'] = 30;
    else f.catalog.splice(0, 1);
    assert.equal(await f.capturer.apply(), false); assert.match(f.ctl.state.message, /saved scene changed/);
    assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('unrelated saved scene edits do not prevent the reviewed capture', async () => {
  const f = await fixture().ready(); await review(f); f.catalog[0].saved++;
  assert.equal(await f.capturer.apply(), true); assert.equal(f.writes().length, 1);
});

test('capture observes current playback at confirmation while retaining the reviewed house grouping', async () => {
  const f = await fixture().ready(); await review(f); f.rows.get('Main room@S2').transport_state = 'paused';
  assert.equal(await f.capturer.apply(), true); assert.equal(f.capturer.state.result.groups[0].playing, false);
  assert.equal(f.writes()[0].method, 'PUT');
});

test('whole-house membership, coordinator and household changes reject the capture', async () => {
  for (const change of [f => f.zones[1].members.push('New room'), f => { f.zones[0].coordinator_room = 'Side room'; },
    f => { f.zones[1].household = 'Changed'; }, f => f.zones.push({ ...f.zones[1], household: 'Added' })]) {
    const f = await fixture().ready(); await review(f); change(f);
    assert.equal(await f.capturer.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('editing the name or queue option, going back, and expired observations retire prior confirmations', async () => {
  const f = await fixture().ready(), old = await review(f);
  f.capturer.edit('name', 'Another name'); assert.equal(await f.capturer.apply(old), false);
  const second = f.capturer.review(); f.capturer.edit('backupQueues', true); assert.equal(await f.capturer.apply(second), false);
  const third = f.capturer.review(); f.capturer.cancelReview(); assert.equal(await f.capturer.apply(third), false);
  f.capturer.review(); f.time(1000 + Scenes.MAX_AGE); f.capturer.observe();
  assert.equal(f.capturer.state.pending, null); assert.equal(f.capturer.canReview(), false); assert.equal(f.writes().length, 0);
});

test('a capture review cannot age out during delayed preflight and still write', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); let waiting = false;
  f.read(async (url, options, normal) => { if (url === '/scenes') { waiting = true; await gate.promise; } return normal(url); });
  const saving = f.capturer.apply(); await settled(() => waiting);
  f.time(1000 + Scenes.MAX_AGE); gate.resolve();
  assert.equal(await saving, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
});

for (const action of ['close', 'suspend', 'dispose', 'selection', 'invalidate']) {
  test(`${action} during preflight suppresses the unsent capture`, async () => {
    const f = await fixture().ready(); await review(f); const gate = deferred(); let waiting = false;
    f.read(async (url, options, normal) => { if (url === '/scenes') { waiting = true; await gate.promise; } return normal(url); });
    const saving = f.capturer.apply(); await settled(() => waiting);
    if (action === 'selection') { f.ctl.state.selected = 'Main room@S1'; f.capturer.observe(); }
    else if (action === 'invalidate') f.ctl.invalidate();
    else f.capturer[action]();
    gate.resolve(); assert.equal(await saving, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  });
}

test('closing an on-demand read aborts its signal and prevents a late catalog from reviving the panel', async () => {
  const f = await fixture().ready(), gate = deferred(); let options;
  f.read(async (url, received, normal) => { if (url === '/scenes') { options = received; await gate.promise; } return normal(url); });
  const opening = f.capturer.open(); await settled(() => options); f.capturer.close();
  assert.equal(options.signal.aborted, true); gate.resolve(); assert.equal(await opening, false);
  assert.equal(f.capturer.state.open, false); assert.equal(f.capturer.state.groups, null); assert.equal(f.capturer.state.pending, null);
});

test('the shared mutation lane blocks transport while the capture is being checked', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); let waiting = false;
  f.read(async (url, options, normal) => { if (url === '/scenes') { waiting = true; await gate.promise; } return normal(url); });
  const saving = f.capturer.apply(); await settled(() => waiting);
  assert.equal(await f.ctl.command('pause'), false); assert.equal(await f.capturer.apply(), false);
  gate.resolve(); assert.equal(await saving, true); assert.equal(f.writes().length, 1);
});

test('lost capture replies retain global uncertainty across closing, reopening and background refresh', async () => {
  const f = await fixture().ready(); await review(f);
  f.write((url, body, options, normal) => { normal(url, body, options); throw new Error('Capture reply lost'); });
  assert.equal(await f.capturer.apply(), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.capturer.state.result, null);
  f.capturer.close(); f.capturer.suspend(); f.capturer.resume(); await f.ctl.refresh(); await f.capturer.open();
  assert.equal(f.capturer.canReview(), false); assert.equal(await f.ctl.command('pause'), false); assert.equal(f.writes().length, 1);
  await f.ctl.refresh(true); await f.capturer.read(); assert.equal(f.capturer.canReview(), true);
  assert.equal(f.catalog.length, 2); assert.equal(f.writes().length, 1);
});

test('wrong scene names, incomplete summaries and wrong queue capture modes cannot publish success', async () => {
  for (const corrupt of [value => ({ ...value, name: 'Wrong scene' }), value => ({ ...value, saved: null }),
    value => ({ ...value, groups: [{ ...value.groups[0], source: { kind: 'queue_backup', queue: { ...value.groups[0].source.queue, total: 12 } } }] })]) {
    const f = await fixture().ready(); await review(f);
    f.write((url, body, options, normal) => corrupt(normal(url, body, options)));
    assert.equal(await f.capturer.apply(), false); assert.equal(f.capturer.state.result, null);
    assert.equal(f.ctl.state.uncertain, true); assert.equal(f.saved.length, 0); assert.equal(f.writes().length, 1);
  }
  const f = await fixture().ready(); await review(f, 'Backup', true);
  f.write((url, body, options, normal) => normal(url, {}, options));
  assert.equal(await f.capturer.apply(), false); assert.equal(f.ctl.state.uncertain, true);
});

test('a failed catalog refresh after accepted storage does not erase success or repeat the capture', async () => {
  const f = await fixture().ready(); await review(f); f.afterSave(() => { throw new Error('Catalog unavailable'); });
  assert.equal(await f.capturer.apply(), true); assert.equal(f.ctl.state.uncertain, false); assert.ok(f.capturer.state.result);
  assert.match(f.capturer.state.message, /Refresh the scene list/); assert.equal(f.writes().length, 1);
});

test('closing after PUT dispatch still records accepted storage and refreshes the catalog once', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); let submitted = false;
  f.write(async (url, body, options, normal) => { const value = normal(url, body, options); submitted = true; await gate.promise; return value; });
  const saving = f.capturer.apply(); await settled(() => submitted); f.capturer.close();
  assert.equal(f.capturer.state.result, null); gate.resolve(); assert.equal(await saving, true);
  assert.ok(f.capturer.state.result); assert.equal(f.capturer.state.open, false); assert.equal(f.saved.length, 1); assert.equal(f.writes().length, 1);
});

test('malformed inventories and catalogs cannot authorize saving, and demo remains read-only', async () => {
  for (const path of ['/zones', '/scenes']) {
    const f = await fixture().ready(); f.read((url, options, normal) => url === path ? {} : normal(url));
    assert.equal(await f.capturer.open(), false); f.capturer.edit('name', 'New'); assert.equal(f.capturer.review(), null); assert.equal(f.writes().length, 0);
  }
  const f = await fixture({ demo: true }).ready(); assert.equal(await f.capturer.open(), true);
  f.capturer.edit('name', 'Demo scene'); assert.equal(f.capturer.review(), null); assert.equal(await f.capturer.apply(), false); assert.equal(f.writes().length, 0);
});

test('actual panel handlers capture a queue backup, show its saved contents and hand recall to existing review', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  get('open-capture-scene').focus(); get('open-capture-scene').fire('click'); await settled(() => f.capturer.state.groups);
  assert.equal(get('capture-scene-dialog').open, true); assert.equal(get('capture-scene-groups').children.length, 2);
  assert.match(get('capture-scene-groups').children[1].textContent, /Main room · S1/);
  get('capture-scene-name').value = 'Later'; get('capture-scene-name').fire('input');
  get('capture-scene-backup').checked = true; get('capture-scene-backup').fire('change');
  get('capture-scene-review').focus(); get('capture-scene-review').fire('click');
  assert.equal(f.doc.activeElement, get('capture-scene-preview')); assert.match(get('capture-scene-preview-title').textContent, /Create.*Later/);
  assert.match(get('capture-scene-preview-detail').textContent, /1,000 entries and 4 MiB/);
  assert.match(get('capture-scene-preview-detail').textContent, /undo cannot recover/); assert.equal(f.writes().length, 0);
  get('capture-scene-confirm').fire('click'); await settled(() => f.saved.length === 1);
  assert.equal(f.writes().length, 1); assert.equal(get('capture-scene-result').hidden, false);
  assert.match(get('capture-scene-result-detail').children[0].children[0].textContent, /12 entries.*track #2/);
  assert.equal(get('capture-scene-recall').disabled, false); get('capture-scene-recall').fire('click');
  assert.deepEqual(f.recalled, ['Later']); assert.equal(get('capture-scene-dialog').open, false);
  assert.equal(f.doc.activeElement, get('open-capture-scene')); assert.equal(f.writes().length, 1);
});

test('actual replacement panel exposes prior groups and levels and edits retire the confirmation', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  await review(f, 'dinner');
  assert.equal(get('capture-scene-existing').hidden, false); assert.equal(get('capture-scene-confirm').textContent, 'Replace saved scene');
  const content = get('capture-scene-existing-detail').children[0].children.map(node => node.textContent).join(' ');
  assert.match(content, /Main room@S1/); assert.match(content, /Room levels:.*Side room@S2: 15/);
  get('capture-scene-name').value = 'x'.repeat(65); get('capture-scene-name').fire('input');
  assert.equal(get('capture-scene-preview').hidden, true); assert.equal(get('capture-scene-review').disabled, true);
  get('capture-scene-dialog').fire('cancel'); assert.equal(f.capturer.state.open, false); assert.equal(f.writes().length, 0);
});

test('real loopback HTTP captures a native PUT once and never applies the saved scene implicitly', async t => {
  let backend; const received = [];
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : undefined; received.push({ url: req.url, method: req.method, body });
    const value = await backend(req.url, body, req.method === 'GET' ? undefined : { method: req.method });
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await fixture({ network(raw) { backend = raw; return C.request({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) }); } }).ready();
  await review(f, 'Quiet / evening', true); assert.equal(await f.capturer.apply(), true);
  assert.deepEqual(received.filter(row => row.method !== 'GET'), [{ url: '/scenes/Quiet%20%2F%20evening', method: 'PUT', body: { backup_queues: true, expected_revision: 'absent' } }]);
  assert.equal(f.ctl.state.uncertain, false); assert.equal(f.capturer.state.result.name, 'Quiet / evening');
});

test('missing or malformed scene revisions cannot enable dashboard storage', async () => {
  for (const revision of [undefined, null, '', 'absent', 42, 'bad\nrevision']) {
    const f = await fixture().ready(); f.catalog[0].revision = revision;
    assert.equal(await f.capturer.open(), false);
    f.capturer.edit('name', 'Dinner');
    assert.equal(f.capturer.canReview(), false); assert.equal(f.capturer.review(), null);
    assert.equal(f.writes().length, 0);
    assert.match(f.capturer.state.message, /revision|^$/);
  }
});

test('private metadata revisions invalidate dashboard replacements before dispatch', async () => {
  const f = await fixture().ready(), pending = await review(f, 'Dinner');
  assert.equal(pending.expectedRevision, f.catalog[0].revision);
  f.catalog[0].revision = 'scene-v1:hidden-change';
  assert.equal(await f.capturer.apply(pending), false);
  assert.equal(f.writes().length, 0); assert.equal(f.capturer.state.pending, null);
  assert.match(f.capturer.state.message, /changed/);
});

test('dashboard create, capture and selected-track reviews retain their proofs through final-write conflicts', async () => {
  for (const kind of ['create', 'capture', 'selection']) {
    const f = await fixture().ready();
    let pending;
    if (kind === 'selection') {
      assert.equal(await f.capturer.openSelection(selectedMusic()), true);
      f.capturer.edit('name', 'Dinner'); pending = f.capturer.review();
    } else pending = await review(f, kind === 'create' ? 'New scene' : 'Dinner');
    const expected = kind === 'create' ? 'absent' : f.catalog[0].revision;
    f.write((url, body) => {
      assert.equal(body.expected_revision, expected);
      const winner = f.capture(pending.name, false); winner.revision = 'scene-v1:concurrent-winner';
      const index = f.catalog.findIndex(scene => scene.name === pending.name);
      if (index < 0) f.catalog.push(winner); else f.catalog[index] = winner;
      throw Object.assign(new Error('The saved scene changed after it was read.'), { code: 'CONFIG_CHANGED', status: 409 });
    });
    assert.equal(await f.capturer.apply(pending), false);
    assert.equal(await f.capturer.apply(pending), false);
    assert.equal(f.writes().length, 1); assert.equal(f.capturer.state.pending, null);
    assert.equal(f.capturer.state.result, null); assert.equal(f.capturer.state.stale, true);
    assert.equal(f.catalog.find(scene => scene.name === pending.name).revision, 'scene-v1:concurrent-winner');
    assert.match(f.capturer.state.message, /changed.*no retry/s);
  }
});
