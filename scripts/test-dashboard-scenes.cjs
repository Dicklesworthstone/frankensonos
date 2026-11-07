'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Scenes = require('../crates/fsonos-api/src/remote/dashboard-scenes.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 200 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, children: [], open: false, hidden: false, disabled: false, checked: false, textContent: '', value: '',
      attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; } };
  }, getElementById(id) {
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
  const capture = (name, backupQueues) => ({ name, saved: 1700000000, groups: zones.map(zone => ({
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
    if (url.startsWith('/zones/') && url.endsWith('/state')) return copy(rows.get(decodeURIComponent(url.slice(7, -6))));
    throw new Error('Unexpected fixture read: ' + url);
  };
  const normalWrite = (url, body, options) => {
    assert.equal(options?.method, 'PUT'); assert.match(url, /^\/scenes\/[^/]+$/);
    const result = capture(decodeURIComponent(url.slice(8)), body.backup_queues === true);
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
  return { ctl, capturer, zones, rows, catalog, calls, saved, recalled, doc, capture, normalRead, normalWrite, session,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body, options }) => ({ url, body, method: options?.method })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; }, afterSave(fn) { onSaved = fn; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}
async function review(f, name = 'Quiet / evening', backup = false) {
  assert.equal(await f.capturer.open(), true); f.capturer.edit('name', name); f.capturer.edit('backupQueues', backup);
  const pending = f.capturer.review(); assert.ok(pending); return pending;
}

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
  assert.deepEqual(f.writes(), [{ url: '/scenes/Quiet%20%2F%20evening', body: {}, method: 'PUT' }]);
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
  assert.deepEqual(f.writes(), [{ url: '/scenes/Queue%20for%20later', body: { backup_queues: true }, method: 'PUT' }]);
  const source = f.capturer.state.result.groups[0].source;
  assert.equal(source.kind, 'queue_backup'); assert.equal(source.queue.total, 12); assert.equal(source.queue.track, 2);
  assert.equal(source.queue.position_secs, 30); assert.equal(source.queue.play_mode, 'NORMAL');
});

test('replacement review retains the exact existing definition and canonical stored spelling', async () => {
  const f = await fixture().ready(), p = await review(f, '  dINNeR  ', true);
  assert.equal(p.name, 'Dinner'); assert.equal(p.existing.name, 'Dinner'); assert.ok(Object.isFrozen(p.existing));
  assert.equal(p.existing.groups[0].source.kind, 'queue');
  assert.equal(await f.capturer.apply(), true); assert.equal(f.catalog.length, 1);
  assert.deepEqual(f.writes(), [{ url: '/scenes/Dinner', body: { backup_queues: true }, method: 'PUT' }]);
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
  assert.deepEqual(received.filter(row => row.method !== 'GET'), [{ url: '/scenes/Quiet%20%2F%20evening', method: 'PUT', body: { backup_queues: true } }]);
  assert.equal(f.ctl.state.uncertain, false); assert.equal(f.capturer.state.result.name, 'Quiet / evening');
});
