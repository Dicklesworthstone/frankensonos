'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const Scenes = require('../crates/fsonos-api/src/remote/scenes.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Source = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const DashboardScenes = require('../crates/fsonos-api/src/remote/dashboard-scenes.js');
const copy = value => structuredClone(value);
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) {
  for (let i = 0; i < 400 && !check(); i++) await Promise.resolve();
  assert.ok(check(), 'reached the expected read or handoff');
}

function documentFixture() {
  const nodes = new Map(), listeners = new Map();
  const walk = node => [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? walk(child) : [])];
  const doc = { hidden: false, activeElement: null,
    addEventListener(name, fn) { if (!listeners.has(name)) listeners.set(name, new Set()); listeners.get(name).add(fn); },
    removeEventListener(name, fn) { listeners.get(name)?.delete(fn); },
    dispatchEvent(event) { for (const fn of listeners.get(event.type) || []) fn(event); return !event.defaultPrevented; },
    createElement(tag) {
      const handlers = new Map();
      return { tag, tagName: tag.toUpperCase(), ownerDocument: doc, children: [], dataset: {}, attributes: {},
        value: '', textContent: '', disabled: false, hidden: false, checked: false, open: false, isConnected: true,
        append(...values) { this.children.push(...values); }, replaceChildren(...values) { this.children = values; },
        setAttribute(key, value) { this.attributes[key] = String(value); }, getAttribute(key) { return this.attributes[key] ?? null; },
        addEventListener(name, fn) { handlers.set(name, fn); }, removeEventListener(name) { handlers.delete(name); },
        fire(name, event = {}) { const e = { target: this, preventDefault() {}, ...event }; return handlers.get(name)?.(e) ?? this['on' + name]?.(e); },
        contains(other) { return walk(this).includes(other); }, closest() { return null; },
        querySelectorAll(selector) {
          const match = /^\[([^=\]]+)(?:="([^"]*)")?\]$/.exec(selector);
          return walk(this).slice(1).filter(node => match ? node.getAttribute(match[1]) !== null && (match[2] == null || node.getAttribute(match[1]) === match[2]) : node.tag === selector);
        },
        querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
        focus() { doc.activeElement = this; }, scrollIntoView() {}, setSelectionRange() {},
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      };
    },
    getElementById(id) { if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id }); return nodes.get(id); },
    querySelectorAll() { return []; }, createTextNode(text) { return String(text); },
  };
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key === 'class') node.className = value;
      else if (key.startsWith('aria-') || key.startsWith('data-')) node.setAttribute(key, value);
      else node[key] = value;
    }
    node.append(...children); return node;
  };
  return { doc, el, walk };
}

function fixture(household = 'S2') {
  const zones = [
    { household, coordinator_room: 'Den', members: ['Den', 'Hall'], transport_state: 'paused' },
    { household, coordinator_room: 'Office', members: ['Office'], transport_state: 'paused' },
    { household: household === 'S2' ? 'S1' : 'S2', coordinator_room: 'Other', members: ['Other'], transport_state: 'paused' },
  ];
  const f = { household, zones, revision: 'native:initial', title: 'Mixed & repeated', identityVersion: '',
    entries: Array.from({ length: 225 }, (_, index) => ({ title: index === 204 ? 'Service\ntrack | reprise' : `Local ${index + 1}`,
      uri: index === 204 ? 'x-sonos-spotify:spotify%3atrack%3aNativeSelection?sid=29&flags=8232&sn=7'
        : `x-file-cifs://example.invalid/music/${index + 1}.flac` })),
    calls: [], writes: [], rows: [], hook: null, receipt: null };
  f.target = 'Den@' + household;
  f.group = P.groups(zones)[0];
  f.page = (offset, limit, expected) => {
    if (expected && expected !== f.revision) throw new Error('The saved playlist changed. Read tracks again.');
    const items = f.entries.slice(offset, offset + limit).map((track, i) => ({ ...track, position: offset + i + 1 }));
    return { zone: 'Den', playlist: { id: 'SQ:7', title: f.title }, revision: f.revision, total: f.entries.length,
      offset, next_offset: offset + items.length < f.entries.length ? offset + items.length : null, items };
  };
  f.selection = () => {
    let basket = P.togglePlaylistTrack(null, f.page(200, 25), 205, f.group);
    basket = P.togglePlaylistTrack(basket, f.page(0, 100), 1, f.group);
    basket = P.editPlaylistBasket(basket, 'repeat', 0);
    basket = P.editPlaylistBasket(basket, 'down', 1);
    return P.nativeSceneFromBasket(basket, f.group);
  };
  const resolve = target => {
    const match = /^(.*)@(S[12]|HOUSE_S[12])$/.exec(target);
    if (!match) throw new Error('Unknown synthetic room');
    const generation = match[2].replace('HOUSE_', '');
    const zone = zones.find(zone => zone.household === generation && zone.members.includes(match[1]));
    if (!zone) throw new Error('Unknown synthetic room');
    return { target, matched: 'room', rooms: [{ room: match[1], household: generation,
      player: `${generation}_${match[1]}${f.identityVersion}`, coordinator: `${generation}_${zone.coordinator_room}${f.identityVersion}` }] };
  };
  f.call = async (url, body, options = {}) => {
    f.calls.push({ url, body: body === undefined ? undefined : copy(body), options });
    if (f.hook) { const result = await f.hook(url, body, options); if (result !== undefined) return result; }
    const parsed = new URL(url, 'http://localhost'), q = Object.fromEntries(parsed.searchParams);
    if (body !== undefined) {
      assert.equal(options.method, 'PUT'); assert.match(parsed.pathname, /^\/scenes\//, 'authoring performs only one stored scene write');
      f.writes.push({ url, body: copy(body), method: options.method });
      const name = decodeURIComponent(parsed.pathname.slice(8)), previous = f.rows.find(row => row.name.toLowerCase() === name.toLowerCase());
      assert.equal(body.expected_revision, previous?.revision || 'absent');
      const { expected_revision, ...definition } = body;
      const result = { name, saved: 1900000000, revision: 'scene:stored', ...copy(definition) };
      for (const group of result.groups) {
        const source = group.source;
        if (source.kind === 'sonos_playlist_tracks') {
          assert.deepEqual(Object.keys(source).sort(), ['kind', 'name', 'selection']);
          assert.equal(source.name, 'SQ:7'); assert.equal(source.selection.zone, f.target);
          assert.equal(source.selection.expected_revision, f.revision);
          group.source = { kind: source.kind, name: source.name,
            playlist: { household: 'HOUSE_' + household, id: 'SQ:7', title: f.title },
            tracks: source.selection.positions.map(position => { const track = f.entries[position - 1]; return { source_uri: track.uri, title: track.title }; }) };
        }
        group.coordinator = group.coordinator.replace(/@S([12])$/, '@HOUSE_S$1');
        group.members = group.members.map(room => room.replace(/@S([12])$/, '@HOUSE_S$1'));
      }
      if (f.receipt) f.receipt(result);
      f.rows = [copy(result)]; return copy(result);
    }
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/scenes') return copy(f.rows);
    if (parsed.pathname === '/rooms/resolve') return resolve(q.target);
    if (parsed.pathname === '/sonos/playlists') return [{ id: 'SQ:7', title: f.title }];
    if (parsed.pathname === '/sonos/playlists/contents') {
      assert.equal(q.zone, f.target, 'native reads remain at the original source group');
      assert.equal(q.playlist, 'SQ:7');
      assert.ok(Number(q.limit) >= 1 && Number(q.limit) <= 100);
      return f.page(Number(q.offset), Number(q.limit), q.expected_revision);
    }
    if (parsed.pathname.startsWith('/zones/') && parsed.pathname.endsWith('/state')) {
      const target = decodeURIComponent(parsed.pathname.slice(7, -6)), zone = zones.find(zone => Core.key(zone) === target);
      return { zone: copy(zone), transport_state: 'paused', volume: 18, mute: false,
        track: { uri: 'fixture:current', title: 'Current song', queue_position: 1, position_secs: 12, duration_secs: 180 } };
    }
    throw new Error('Unexpected read ' + url);
  };
  f.namedCall = (name, options = {}) => {
    const route = { ...P.routes, ...Scenes.routes, zones: ['GET', '/zones'], scenes: ['GET', '/scenes'] }[name];
    assert.ok(route, 'known route ' + name);
    let url = route[1].replace(/\{([^}]+)\}/g, (_, key) => encodeURIComponent(options.params?.[key] || ''));
    if (options.query) url += '?' + new URLSearchParams(options.query);
    return f.call(url, options.body, { method: route[0], signal: options.signal });
  };
  f.sourceReads = () => f.calls.filter(call => call.url.startsWith('/sonos/playlists/contents?'));
  f.openPhoneScene = async () => {
    f.scenes = Scenes.model({ call: f.namedCall });
    assert.equal(f.scenes.offerSelection(f.selection()), true); await tick();
    assert.ok(f.scenes.state.draft, f.scenes.state.message);
    f.scenes.editName('Native evening');
    return f.scenes;
  };
  f.openDashboard = async dom => {
    let scene, source;
    f.controller = Core.model({ call: f.call, now: () => 1000, changed: () => { scene?.observe(); source?.observe(); } });
    await f.controller.refresh(); f.controller.connection(true); f.controller.select(f.target);
    const session = { state: { scenes: [] }, async catalog() { this.state.scenes = copy(f.rows); } };
    const opts = { call: f.call, controller: f.controller, now: () => 1000, document: dom?.doc, session };
    scene = dom ? DashboardScenes.panel(opts) : DashboardScenes.model(opts);
    source = dom ? Source.panel(opts) : Source.model(opts);
    f.dashboardScene = scene; f.dashboardSource = source;
    return { scene, source };
  };
  return f;
}

for (const household of ['S1', 'S2']) for (const playing of [true, false]) {
  test(`phone authors exact mixed native occurrences on ${household} with playing=${playing}`, async () => {
    const f = fixture(household), scene = await f.openPhoneScene();
    scene.editGroup(scene.state.draft.groups[0], 'playing', playing ? 'play' : 'pause');
    assert.equal(await scene.reviewDefinition(), true, scene.state.message);
    const pending = scene.state.pending;
    assert.deepEqual(pending.prepared.body.groups[0].source, { kind: 'sonos_playlist_tracks', name: 'SQ:7',
      selection: { zone: f.target, positions: [205, 1, 205], expected_revision: f.revision } });
    assert.deepEqual(f.writes, []);
    assert.equal(await scene.confirm(pending), true, scene.state.message);
    assert.equal(f.writes.length, 1); assert.equal(f.writes[0].body.groups[0].playing, playing);
    assert.deepEqual(scene.state.saved.groups[0].source.tracks.map(track => track.source_uri), f.selection().tracks.map(track => track.source_uri));
    assert.equal(f.sourceReads().length, 6, 'two retained ranges checked on import, review and confirmation');
    assert.equal(await scene.confirm(pending), false); assert.equal(f.writes.length, 1);
  });

  test(`dashboard authors exact mixed native occurrences on ${household} with playing=${playing}`, async () => {
    const f = fixture(household), { scene } = await f.openDashboard();
    assert.equal(await scene.openSelection(f.selection()), true, scene.state.message);
    scene.edit('name', 'Native evening'); scene.edit('playing', playing);
    const pending = await scene.review(); assert.ok(pending, scene.state.message);
    assert.deepEqual(f.writes, []);
    assert.equal(await scene.apply(pending), true, scene.state.message);
    assert.equal(f.writes.length, 1); assert.equal(f.writes[0].body.groups[0].playing, playing);
    assert.deepEqual(f.writes[0].body.groups[0].source.selection.positions, [205, 1, 205]);
    assert.equal(f.sourceReads().length, 6);
    assert.equal(await scene.apply(pending), false); assert.equal(f.writes.length, 1);
  });
}

test('phone can change the scene destination without changing the native source reader', async () => {
  const f = fixture(), scene = await f.openPhoneScene();
  scene.editGroup(scene.state.draft.groups[0], 'coordinator', 'Office@S2');
  assert.equal(scene.state.draft.groups[0].kind, 'sonos_playlist_tracks');
  assert.equal(await scene.reviewDefinition(), true, scene.state.message);
  const pending = scene.state.pending;
  assert.equal(pending.prepared.body.groups[0].coordinator, 'Office@S2');
  assert.equal(pending.prepared.body.groups[0].source.selection.zone, 'Den@S2');
  assert.equal(await scene.confirm(pending), true, scene.state.message);
  assert.ok(f.sourceReads().every(call => new URL(call.url, 'http://localhost').searchParams.get('zone') === 'Den@S2'));
});

test('a phone native selection cannot be authored into another household', async () => {
  const f = fixture(), scene = await f.openPhoneScene();
  scene.editGroup(scene.state.draft.groups[0], 'coordinator', 'Other@S1');
  assert.equal(await scene.reviewDefinition(), false); assert.match(scene.state.message, /original household/);
  assert.deepEqual(f.writes, []);
});

for (const surface of ['phone', 'dashboard']) for (const stage of ['review', 'save']) {
  test(`${surface} rechecks native revision before ${stage} and sends no stale save`, async () => {
    const f = fixture();
    let scene, pending;
    if (surface === 'phone') scene = await f.openPhoneScene();
    else { ({ scene } = await f.openDashboard()); await scene.openSelection(f.selection()); scene.edit('name', 'Native evening'); }
    if (stage === 'save') pending = surface === 'phone' ? (await scene.reviewDefinition(), scene.state.pending) : await scene.review();
    f.revision = 'native:changed';
    const result = surface === 'phone' ? stage === 'review' ? await scene.reviewDefinition() : await scene.confirm(pending)
      : stage === 'review' ? await scene.review() : await scene.apply(pending);
    assert.ok(!result); assert.deepEqual(f.writes, []);
  });
}

for (const surface of ['phone', 'dashboard']) for (const change of ['resource', 'title', 'repeat', 'household', 'destination']) {
  test(`${surface} rejects a changed ${change} receipt after one native scene save`, async () => {
    const f = fixture();
    f.receipt = result => {
      const source = result.groups[0].source;
      if (change === 'resource') source.tracks[0].source_uri = 'x-file-cifs://example.invalid/other.flac';
      if (change === 'title') source.tracks[0].title = 'Different display';
      if (change === 'repeat') source.tracks.pop();
      if (change === 'household') source.playlist.household = 'OtherHousehold';
      if (change === 'destination') result.groups[0].coordinator = 'Office@HOUSE_S2';
    };
    let scene, pending, result;
    if (surface === 'phone') {
      scene = await f.openPhoneScene(); await scene.reviewDefinition(); pending = scene.state.pending;
      result = await scene.confirm(pending); assert.equal(await scene.confirm(pending), false);
    } else {
      ({ scene } = await f.openDashboard()); await scene.openSelection(f.selection()); scene.edit('name', 'Native evening');
      pending = await scene.review(); result = await scene.apply(pending); assert.equal(await scene.apply(pending), false);
    }
    assert.equal(result, false); assert.equal(f.writes.length, 1);
    assert.match(scene.state.message, /uncertain|not confirmed/i);
  });
}

for (const surface of ['phone', 'dashboard']) {
  test(`${surface} refuses changed source grouping before a native save`, async () => {
    const f = fixture(); let scene, pending;
    if (surface === 'phone') { scene = await f.openPhoneScene(); await scene.reviewDefinition(); pending = scene.state.pending; }
    else { ({ scene } = await f.openDashboard()); await scene.openSelection(f.selection()); scene.edit('name', 'Native evening'); pending = await scene.review(); }
    f.zones[0].members = ['Den'];
    const result = surface === 'phone' ? await scene.confirm(pending) : await scene.apply(pending);
    assert.equal(result, false); assert.deepEqual(f.writes, []);
  });

  test(`${surface} closing while a native range is read prevents publication and writes`, async () => {
    const f = fixture(); let scene;
    if (surface === 'phone') scene = await f.openPhoneScene();
    else { ({ scene } = await f.openDashboard()); await scene.openSelection(f.selection()); scene.edit('name', 'Native evening'); }
    const wait = deferred(); let entered = false;
    f.hook = url => { if (url.startsWith('/sonos/playlists/contents?')) { entered = true; return wait.promise; } };
    const work = surface === 'phone' ? scene.reviewDefinition() : scene.review();
    await until(() => entered); scene.close(); wait.resolve(f.page(0, 1, f.revision)); await work;
    assert.equal(scene.state.pending, null); assert.deepEqual(f.writes, []);
  });
}

async function sourceBasket(f, source, phone) {
  if (phone) { await source.refresh(); source.edit('target', f.target); }
  else source.open();
  source.edit('sourceType', 'playlist'); await source.loadPlaylists(); source.edit('playlist', 'SQ:7');
  await source.loadContents(); source.selectTrack(1);
  await source.loadContents('next'); await source.loadContents('next'); source.selectTrack(205);
  source.changeTrackSelection('up', 1); source.changeTrackSelection('repeat', 0); source.changeTrackSelection('down', 1);
}

for (const surface of ['phone', 'dashboard']) {
  test(`${surface} source model hands off its current ordered basket without playback and refuses a separate room-set destination`, async () => {
    const f = fixture(); let source;
    if (surface === 'phone') source = P.model({ call: f.namedCall }); else ({ source } = await f.openDashboard());
    await sourceBasket(f, source, surface === 'phone');
    const before = f.sourceReads().length, selection = await source.sceneSelection();
    assert.ok(selection, source.state.message); assert.deepEqual(selection.native.item.positions, [205, 1, 205]);
    assert.equal(f.sourceReads().length - before, 2); assert.deepEqual(f.writes, []);
    source.edit('selectionTarget', 'all@S2');
    assert.equal(await source.sceneSelection(), null); assert.match(source.state.message, /Clear Selected-track destination/);
    source.edit('selectionTarget', ''); f.revision = 'native:replaced';
    assert.equal(await source.sceneSelection(), null); assert.deepEqual(f.writes, []);
  });
}

test('shared selected-track controls name the scene group and never route a room-set playback target into scene creation', async () => {
  const f = fixture(), dom = documentFixture(), page = f.page(0, 100), group = f.group;
  const basket = P.togglePlaylistTrack(null, page, 1, group); let offered = 0;
  const panel = P.playlistTrackSelectionPanel(dom.doc, { review() {}, clear() {}, destination() {}, scene() { offered++; } });
  panel.render(page, [1], false, { group, basket, target: '' });
  const button = dom.walk(panel.node).find(node => node.textContent === 'Save selection as scene');
  assert.ok(button); assert.equal(button.disabled, false); button.fire('click'); assert.equal(offered, 1);
  assert.ok(dom.walk(panel.node).some(node => /Save these exact occurrences for Den@S2/.test(node.textContent)));
  panel.render(page, [1], false, { group, basket, target: 'all@S2' });
  assert.equal(button.disabled, true); button.fire('click'); assert.equal(offered, 1);
  assert.ok(dom.walk(panel.node).some(node => /Clear Selected-track destination/.test(node.textContent)));
});

test('the actual dashboard source button opens native scene authoring and saves through the scene dialog', async () => {
  const dom = documentFixture(), f = fixture(), { source, scene } = await f.openDashboard(dom);
  await sourceBasket(f, source, false);
  const save = dom.walk(dom.doc.getElementById('source-contents')).find(node => node.textContent === 'Save selection as scene');
  assert.ok(save); assert.equal(save.disabled, false); save.fire('click');
  await until(() => scene.state.open && !scene.state.loading);
  assert.equal(source.state.open, false); assert.equal(scene.state.selection.kind, 'sonos_playlist_tracks');
  assert.match(dom.doc.getElementById('capture-scene-tracks').textContent, /Mixed & repeated/);
  const name = dom.doc.getElementById('capture-scene-name'); name.value = 'Native from dashboard'; name.fire('input');
  dom.doc.getElementById('capture-scene-playback').value = 'pause'; dom.doc.getElementById('capture-scene-playback').fire('change');
  await dom.doc.getElementById('capture-scene-review').fire('click'); await until(() => !!scene.state.pending);
  dom.doc.getElementById('capture-scene-confirm').fire('click'); await until(() => !scene.state.writing);
  assert.equal(f.writes.length, 1); assert.equal(f.writes[0].body.groups[0].playing, false);
  assert.ok(scene.state.result, scene.state.message);
  scene.dispose();
});

test('the actual phone scene listener presents native occurrences without editable renderer metadata', async t => {
  const dom = documentFixture(), f = fixture(), previousDocument = global.document;
  global.document = dom.doc;
  t.after(() => { global.document = previousDocument; });
  const panel = Scenes.panel({ call: f.namedCall, el: dom.el });
  const event = new CustomEvent('fsonos:scene-selection', { cancelable: true, detail: f.selection() });
  assert.equal(dom.doc.dispatchEvent(event), false); await tick();
  const node = key => dom.walk(panel.node).find(node => node.getAttribute('data-scene-focus') === key);
  assert.equal(node('source-1').value, 'sonos_playlist_tracks');
  assert.ok(dom.walk(panel.node).some(node => node.getAttribute('aria-label') === 'Group 1 native tracks in playback order'
    && node.textContent.includes('Service\ntrack | reprise')));
  assert.equal(node('tracks-1'), undefined, 'native resource metadata is not pasted or authored');
  assert.ok(!node('leader-1').children.some(option => option.value === 'Other@S1'));
  node('name').value = 'Native from phone'; node('name').fire('input');
  node('review-definition').fire('click'); await until(() => !!node('confirm'));
  node('confirm').fire('click'); await until(() => f.writes.length === 1); await tick();
  assert.equal(f.writes[0].body.groups[0].source.selection.zone, f.target);
  panel.dispose();
});
