// node --test scripts/test-playlist-save-remote.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const Remote = require('../crates/fsonos-api/src/remote/playlist-save.js');
const Save = require('../crates/fsonos-api/src/remote/dashboard-playlist-save.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Player = require('../crates/fsonos-api/src/remote/source-player.js');
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };

function fixture({ demo = false } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Den',
    members: household === 'S2' ? ['Den', 'Side'] : ['Den'], transport_state: 'playing' }));
  const cards = new Map(zones.map(zone => [C.key(zone), { zone, name: C.key(zone), writing: false, disposed: false }]));
  const queues = new Map(zones.map(zone => [C.key(zone), { zone: 'Den', total: 3, revision: 'queue:' + zone.household,
    playing: 2, items: [{ position: 1, uri: 'fixture:first', title: 'First song' }] }]));
  const catalogs = new Map(zones.map(zone => [C.key(zone), [{ id: 'SQ:7', title: 'Existing mix' }]]));
  const calls = [], flights = new Map(), begins = [], ends = [];
  let hook = null, control, adapter, time = 1000;
  const current = parts => !parts.disposed && cards.get(C.key(parts.zone)) === parts;
  const members = parts => parts.zone.members.map(room => room + '@' + parts.zone.household);
  const busy = parts => members(parts).some(room => flights.has(room));
  const ordinary = (name, args = {}) => {
    if (name === 'zones') return structuredClone(zones);
    if (name === 'state') {
      const zone = zones.find(zone => C.key(zone) === args.params.room);
      return { zone: structuredClone(zone), transport_state: 'playing', volume: 17, mute: false,
        track: { uri: 'fixture:radio', title: 'Radio with a retained queue', position_secs: 40, duration_secs: 300 } };
    }
    if (name === 'queue') return structuredClone(queues.get(args.params.room));
    if (name === 'nativePlaylistCatalog') return structuredClone(catalogs.get(args.query.zone));
    if (name === 'nativePlaylistSave') {
      const playlist = { id: 'SQ:' + (catalogs.get(args.body.zone).length + 7), title: args.body.title };
      catalogs.get(args.body.zone).push(playlist);
      return { zone: 'Den', done: 'Native queue saved', logged: true, playlist };
    }
    throw new Error('Unexpected route: ' + name);
  };
  const call = async (name, args) => {
    calls.push({ name, args });
    if (hook) { const value = hook(name, args, ordinary); if (value !== undefined) return value; }
    return ordinary(name, args);
  };
  const begin = parts => {
    if (!current(parts) || parts.writing || busy(parts)) return false;
    parts.writing = true; begins.push(parts);
    for (const room of members(parts)) flights.set(room, parts);
    adapter.observe(parts);
    return true;
  };
  const end = parts => {
    ends.push(parts); parts.writing = false;
    for (const [room, owner] of flights) if (owner === parts) flights.delete(room);
    adapter.observe(parts);
  };
  adapter = Remote.bridge({ call, cards: () => cards.values(), current, busy, begin, end, demo,
    now: () => time, changed: () => control?.observe() });
  control = Save.model({ call: adapter.call, controller: adapter.controller, now: () => time });
  return { zones, cards, queues, catalogs, calls, flights, begins, ends, adapter, control, call, current, busy, begin, end,
    now: () => time, advance(ms) { time += ms; },
    hook(value) { hook = value; },
    writes: () => calls.filter(row => row.name === 'nativePlaylistSave'),
    async ready(target = 'Den@S2') {
      assert.equal(await adapter.choose(cards.get(target).zone), true);
      assert.equal(control.open(), true); assert.equal(await control.read(), true);
      assert.equal(control.edit('title', 'Evening mix'), true);
      const pending = await control.review(); assert.ok(pending); return pending;
    },
  };
}

test('unopened phone save controls are lazy and expose only the native save routes', () => {
  const f = fixture();
  assert.equal(f.calls.length, 0); assert.equal(f.begins.length, 0);
  assert.deepEqual(Remote.routes.nativePlaylistSave, ['POST', '/sonos/playlists/save']);
  assert.throws(() => f.adapter.call('/pause', { zone: 'Den@S2' }), /not supported/);
  assert.throws(() => f.adapter.call('/sonos/playlists/save?retry=1', {}), /not supported/);
  assert.equal(f.calls.length, 0);
});

test('the phone saves the exact same-named S1 group with one shared native command and keeps radio playing', async () => {
  const f = fixture(), queue = structuredClone(f.queues.get('Den@S1'));
  await f.ready('Den@S1');
  assert.equal(f.writes().length, 0);
  assert.equal(await f.control.apply(), true);
  assert.deepEqual(f.writes().map(row => row.args.body), [{ zone: 'Den@S1', title: 'Evening mix' }]);
  assert.equal(f.control.state.result.playlist.id, 'SQ:8');
  assert.equal(f.control.state.result.target, 'Den@S1');
  assert.equal(f.begins.length, 1); assert.equal(f.ends.length, 1); assert.equal(f.flights.size, 0);
  assert.deepEqual(f.queues.get('Den@S1'), queue);
  assert.ok(f.calls.every(row => ['zones', 'state', 'queue', 'nativePlaylistSave'].includes(row.name)));
});

test('a changed queue revision after review prevents saving and releases the shared phone lane', async () => {
  const f = fixture(); await f.ready();
  f.queues.get('Den@S2').revision = 'same-length-edit';
  assert.equal(await f.control.apply(), false);
  assert.equal(f.writes().length, 0); assert.equal(f.adapter.controller.state.uncertain, false);
  assert.match(f.control.state.result.detail, /queue changed/i);
  assert.equal(f.begins.length, 1); assert.equal(f.ends.length, 1); assert.equal(f.flights.size, 0);
});

test('phone transport ownership blocks a save and is not released by a rejected attempt', async () => {
  const f = fixture(); const pending = await f.ready();
  const parts = f.cards.get('Den@S2'), other = {};
  f.flights.set('Side@S2', other); f.adapter.observe(parts);
  assert.equal(f.control.state.pending, null);
  assert.equal(await f.control.apply(pending), false);
  assert.equal(f.flights.get('Side@S2'), other); assert.equal(f.begins.length, 0); assert.equal(f.writes().length, 0);
});

test('card replacement during final preparation fences the command before its POST', async () => {
  const f = fixture(); await f.ready(); const gate = deferred();
  f.hook((name, args) => name === 'queue' ? gate.promise : undefined);
  const saving = f.control.apply(); await settle();
  assert.equal(f.begins.length, 1); const old = f.cards.get('Den@S2');
  old.disposed = true; f.cards.set('Den@S2', { ...old, disposed: false, writing: false });
  f.adapter.invalidate(old);
  gate.resolve(structuredClone(f.queues.get('Den@S2')));
  assert.equal(await saving, false); assert.equal(f.writes().length, 0);
  assert.equal(f.adapter.controller.state.uncertain, false); assert.equal(f.ends[0], old); assert.equal(f.flights.size, 0);
});

test('a submitted save retains original room ownership through card replacement until its receipt settles', async () => {
  const f = fixture(); await f.ready(); const gate = deferred();
  f.hook((name, args, next) => name === 'nativePlaylistSave' ? gate.promise.then(() => next(name, args)) : undefined);
  const saving = f.control.apply(); await settle();
  assert.equal(f.writes().length, 1); assert.equal(f.flights.size, 2);
  const old = f.cards.get('Den@S2'); old.disposed = true;
  const split = { zone: { household: 'S2', coordinator_room: 'Side', members: ['Side'], transport_state: 'playing' }, writing: false, disposed: false };
  f.cards.set('Side@S2', split); f.adapter.invalidate(old); f.control.close();
  assert.equal(f.busy(split), true); assert.equal(f.ends.length, 0);
  gate.resolve(); assert.equal(await saving, true);
  assert.equal(f.ends[0], old); assert.equal(f.busy(split), false);
  assert.equal(f.control.state.result.target, 'Den@S2'); assert.equal(f.control.state.result.status, 'accepted');
});

test('closing or hiding after submission retains the confirmed save without repeating it', async () => {
  for (const hide of [false, true]) {
    const f = fixture(); await f.ready(); const gate = deferred();
    f.hook((name, args, next) => name === 'nativePlaylistSave' ? gate.promise.then(() => next(name, args)) : undefined);
    const saving = f.control.apply(); await settle();
    if (hide) { f.adapter.suspend(); f.control.suspend(); } else f.control.close();
    assert.equal(f.flights.size, 2);
    gate.resolve(); assert.equal(await saving, true);
    if (hide) { f.adapter.resume(); f.control.resume(); }
    assert.equal(f.control.open(), true);
    assert.equal(f.control.state.result.status, 'accepted'); assert.equal(f.writes().length, 1); assert.equal(f.flights.size, 0);
  }
});

async function lost(f) {
  await f.ready();
  f.hook((name, args, next) => { if (name === 'nativePlaylistSave') { next(name, args); throw new Error('Reply lost'); } });
  assert.equal(await f.control.apply(), false); assert.equal(f.adapter.controller.state.uncertain, true);
  f.hook(null);
}

test('uncertain saves stay scoped across group changes and recover even after the original cards disappear', async () => {
  const f = fixture(); await lost(f);
  assert.equal(await f.adapter.choose(f.cards.get('Den@S1').zone), true);
  f.control.open(); assert.equal(f.control.canRead(), false);
  assert.equal(f.control.state.result.target, 'Den@S2');
  await f.adapter.controller.refresh(true);
  await f.adapter.controller.refreshStored('playlists', Save.decodeCatalog, { explicit: true, storeTarget: 'Den@S1' });
  assert.equal(f.adapter.controller.state.uncertain, true); assert.equal(f.writes().length, 1);
  f.cards.clear(); f.zones.length = 0;
  await f.adapter.controller.refresh(false); f.control.open();
  assert.equal(f.control.canCheckCatalog(), true);
  assert.equal((await f.control.checkCatalog()).length, 2);
  assert.equal(f.calls.at(-1).name, 'nativePlaylistCatalog');
  assert.equal(f.calls.at(-1).args.query.zone, 'Den@S2');
  assert.equal(f.adapter.controller.state.uncertain, false);
  assert.equal(f.control.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
});

test('obsolete or malformed catalog recovery reads cannot release native-save uncertainty', async () => {
  for (const kind of ['event', 'malformed', 'hidden']) {
    const f = fixture(); await lost(f); const gate = deferred();
    f.hook(name => name === 'nativePlaylistCatalog' ? gate.promise : undefined);
    const checking = f.control.checkCatalog(); await settle();
    if (kind === 'event') f.control.catalogChanged({ household: 'S2', playlists: true });
    if (kind === 'hidden') { f.adapter.suspend(); f.control.suspend(); }
    gate.resolve(kind === 'malformed' ? [{ id: 'SQ:8', title: 'A' }, { id: 'SQ:8', title: 'B' }]
      : structuredClone(f.catalogs.get('Den@S2')));
    assert.equal(await checking, null); assert.equal(f.adapter.controller.state.uncertain, true);
    assert.equal(f.writes().length, 1);
  }
});

test('an unrelated household event does not cancel original-catalog recovery after selecting another group', async () => {
  const f = fixture(); await lost(f);
  await f.adapter.choose(f.cards.get('Den@S1').zone); f.control.open();
  const gate = deferred(); f.hook(name => name === 'nativePlaylistCatalog' ? gate.promise : undefined);
  const checking = f.control.checkCatalog(); await settle();
  f.control.catalogChanged({ household: 'S1', playlists: true });
  gate.resolve(structuredClone(f.catalogs.get('Den@S2')));
  assert.equal((await checking).length, 2); assert.equal(f.adapter.controller.state.uncertain, false);
});

test('same-length queue invalidation retires the review and cannot be revived by late phone playback', async () => {
  const f = fixture(); const pending = await f.ready();
  f.adapter.invalidate(f.cards.get('Den@S2'));
  assert.equal(f.control.state.pending, null); assert.equal(f.control.canReview(), false);
  f.adapter.observe(f.cards.get('Den@S2'));
  assert.equal(await f.control.apply(pending), false); assert.equal(f.writes().length, 0);
});

test('a healthy selected group remains usable when another household state read fails', async () => {
  const f = fixture();
  f.hook((name, args) => { if (name === 'state' && args.params.room === 'Den@S1') throw new Error('Other household offline'); });
  await f.ready(); assert.equal(await f.control.apply(), true); assert.equal(f.writes().length, 1);
});

test('demo mode and suspension cannot submit a native save', async () => {
  const f = fixture({ demo: true });
  await f.adapter.choose(f.cards.get('Den@S2').zone); f.control.open(); f.control.edit('title', 'Preview');
  assert.equal(await f.control.read(), false); assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
  const live = fixture(), gate = deferred();
  live.hook(name => name === 'zones' ? gate.promise : undefined);
  const selecting = live.adapter.choose(live.cards.get('Den@S2').zone);
  live.adapter.suspend(); gate.resolve(structuredClone(live.zones));
  assert.equal(await selecting, false); assert.equal(live.writes().length, 0);
});

function sourceFixture() {
  const f = fixture(), calls = [];
  let hook;
  const model = Player.model({ call: async (name, args) => {
    calls.push({ name, args });
    if (hook) { const result = hook(name, args); if (result !== undefined) return result; }
    if (name === 'zones') return structuredClone(f.zones);
    if (name === 'sourcePlaylists') return structuredClone(f.catalogs.get(args.query.zone));
    throw new Error('Browsing cannot call ' + name);
  } });
  const value = { target: 'Den@S1', signature: C.shape(f.zones[1]), playlist: { id: 'SQ:7', title: 'Existing mix' } };
  return { ...f, model, sourceCalls: calls, value, sourceHook(fn) { hook = fn; } };
}

test('browsing a newly saved playlist reads its exact group and native id without starting playback', async () => {
  const f = sourceFixture();
  assert.equal(await f.model.browseSaved(f.value), true);
  assert.equal(f.model.state.sourceType, 'playlist'); assert.equal(f.model.state.target, 'Den@S1');
  assert.equal(f.model.state.playlist, 'SQ:7'); assert.equal(f.model.state.pending, null);
  assert.deepEqual(f.sourceCalls.map(row => row.name), ['zones', 'sourcePlaylists']);
  assert.equal(f.sourceCalls[1].args.query.zone, 'Den@S1');
});

test('the saved-playlist handoff preserves an existing source draft', async () => {
  const f = sourceFixture(); await f.model.refresh();
  f.model.edit('input', 'https://audio.example/live'); f.model.edit('title', 'My station');
  const count = f.sourceCalls.length;
  assert.equal(await f.model.browseSaved(f.value), false);
  assert.equal(f.model.state.input, 'https://audio.example/live'); assert.equal(f.model.state.title, 'My station');
  assert.equal(f.sourceCalls.length, count); assert.match(f.model.state.message, /clear its draft/);
  const searching = sourceFixture(); await searching.model.refresh();
  searching.model.edit('targetQuery', 'Dining');
  assert.equal(await searching.model.browseSaved(searching.value), false);
  assert.equal(searching.model.state.targetQuery, 'Dining');
});

test('changed group, renamed catalog entry, or a hidden handoff cannot select a stale playlist', async () => {
  for (const kind of ['group', 'title', 'hidden']) {
    const f = sourceFixture(), gate = deferred();
    if (kind === 'group') f.zones[1].members.push('New member');
    if (kind === 'title') f.catalogs.get('Den@S1')[0].title = 'Renamed';
    if (kind === 'hidden') f.sourceHook(name => name === 'sourcePlaylists' ? gate.promise : undefined);
    const browsing = f.model.browseSaved(f.value);
    if (kind === 'hidden') { await settle(); f.model.close(); gate.resolve(structuredClone(f.catalogs.get('Den@S1'))); }
    assert.equal(await browsing, false); assert.equal(f.model.state.playlist, ''); assert.equal(f.model.state.pending, null);
  }
});

test('the shipped phone bundle orders shared dependencies and uses only scoped extra styles', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const rust = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  const ordered = ['{SOURCE_PLAYER_SCRIPT}', '{DASHBOARD_CORE_SCRIPT}', '{PLAYLIST_SAVE_SCRIPT}', '{PLAYLIST_SAVE_PANEL_SCRIPT}', '{NATIVE_SAVE_SCRIPT}'];
  for (let i = 1; i < ordered.length; i++) assert.ok(rust.indexOf(ordered[i]) > rust.indexOf(ordered[i - 1]));
  assert.match(rust, /include_str!\("\.\.\/remote\/playlist-save\.js"\)/);
  assert.match(rust, /\{PLAYLIST_SAVE_STYLE\}\\n\{NATIVE_SAVE_STYLE\}/);
  const style = fs.readFileSync(path.join(root, 'remote/playlist-save.css'), 'utf8');
  assert.doesNotMatch(style, /(?:^|\n)\s*(?:body|:root|\*)\s*[{,]/);
  const studio = fs.readFileSync(path.join(root, 'remote/studio.js'), 'utf8');
  assert.match(studio, /id: 'queue-save', workspace: 'music'/);
  assert.match(studio, /fsonos:playlist-browse/);
});

function documentFixture() {
  const nodes = [];
  class Events {
    constructor() { this.events = new Map(); }
    addEventListener(type, fn) { this.events.set(type, [...(this.events.get(type) || []), fn]); }
    removeEventListener(type, fn) { this.events.set(type, (this.events.get(type) || []).filter(item => item !== fn)); }
    dispatchEvent(event) { event.target = this; for (const fn of this.events.get(event.type) || []) fn(event); return !event.defaultPrevented; }
    fire(type, detail) { return this.dispatchEvent(new Event(type, { detail, cancelable: true })); }
  }
  class Event {
    constructor(type, { detail, cancelable = false } = {}) { this.type = type; this.detail = detail; this.cancelable = cancelable; this.defaultPrevented = false; }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
  }
  class Element extends Events {
    constructor(tag) { super(); this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map();
      this.dataset = {}; this.value = ''; this.open = false; this.isConnected = true; this._text = ''; nodes.push(this); }
    set textContent(value) { this._text = String(value); this.children = []; }
    get textContent() { return this._text + this.children.map(node => node.textContent).join(''); }
    append(...children) { for (const child of children) { child.parentElement = this; this.children.push(child); } }
    before(node) { const at = this.parentElement.children.indexOf(this); this.parentElement.children.splice(at, 0, node); node.parentElement = this.parentElement; }
    replaceChildren(...children) { this._text = ''; this.children = []; this.append(...children); }
    setAttribute(key, value) { this.attributes.set(key, String(value)); if (key === 'id') this.id = String(value); if (key === 'disabled' || key === 'hidden') this[key] = true; }
    getAttribute(key) { return this.attributes.get(key) ?? null; }
    click() { if (!this.disabled) { this.focus(); this.fire('click'); } }
    focus() { doc.activeElement = this; }
    showModal() { this.open = true; }
    close() { this.open = false; this.fire('close'); }
  }
  const doc = Object.assign(new Events(), { hidden: false, activeElement: null,
    createElement: tag => new Element(tag), getElementById: id => nodes.find(node => node.id === id),
    querySelector: selector => selector === 'dialog[open]' ? nodes.find(node => node.tagName === 'DIALOG' && node.open) || null : null });
  doc.defaultView = Object.assign(new Events(), { CustomEvent: Event });
  doc.body = new Element('body');
  const host = new Element('main'); host.id = 'households'; doc.body.append(host);
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (value == null || value === false) continue;
      if (key === 'text') node.textContent = value;
      else if (key === 'class') node.className = value;
      else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
      else node.setAttribute(key, value === true ? '' : value);
    }
    node.append(...children); return node;
  };
  return { doc, nodes, el, Event, $: id => doc.getElementById(id) };
}

function mounted() {
  const f = fixture(), ui = documentFixture();
  for (const parts of f.cards.values()) {
    parts.panels = []; parts.queue = { node: ui.el('details') }; ui.$('households').append(parts.queue.node);
  }
  const native = Remote.mount({ call: f.call, cards: () => f.cards.values(), current: f.current, busy: f.busy,
    begin: parts => { const ok = f.begin(parts); if (ok) for (const panel of parts.panels) panel.block(true); return ok; },
    end: parts => { f.end(parts); for (const panel of parts.panels) panel.block(false); },
    register: factory => { for (const parts of f.cards.values()) parts.panels.push(factory(parts)); },
    el: ui.el, document: ui.doc, now: f.now });
  const button = target => f.cards.get(target).queue.node.children[0].children[0];
  return { ...f, ...ui, native, button,
    async open(target = 'Den@S2') {
      button(target).click(); await settle();
      assert.equal(native.panel.control.state.loaded, true);
      ui.$('playlist-save-name').value = 'My retained mix'; ui.$('playlist-save-name').fire('input');
      ui.$('playlist-save-form').fire('submit'); await settle();
      assert.ok(native.panel.control.state.pending);
    },
  };
}

test('mounted phone Queue buttons use the shared dialog and offer a separate saved-catalog handoff', async t => {
  const f = mounted(); t.after(() => f.native.dispose());
  const handoffs = [];
  f.doc.addEventListener('fsonos:playlist-browse', event => { event.preventDefault(); handoffs.push(event.detail.selection); event.detail.response = Promise.resolve(true); });
  assert.equal(f.calls.length, 0); assert.equal(f.$('open-playlist-save').disabled, true);
  await f.open('Den@S1');
  assert.equal(f.$('playlist-save-dialog').open, true);
  assert.match(f.$('playlist-save-preview-target').textContent, /S1/);
  assert.equal(f.writes().length, 0);
  f.$('playlist-save-confirm').click(); await settle();
  assert.equal(f.writes().length, 1); assert.equal(f.$('playlist-save-result-title').textContent, 'Playlist saved');
  assert.equal(f.native.panel.control.state.result.target, 'Den@S1');
  assert.equal(f.$('playlist-save-browse').disabled, false);
  f.$('playlist-save-browse').click(); await settle();
  assert.equal(handoffs.length, 1); assert.equal(handoffs[0].target, 'Den@S1');
  assert.equal(handoffs[0].playlist.id, 'SQ:8'); assert.equal(f.$('playlist-save-dialog').open, false);
  assert.equal(f.writes().length, 1);
});

test('mounted catalog invalidations retire a delayed recovery read without losing the persistent receipt', async t => {
  const f = mounted(); t.after(() => f.native.dispose());
  await f.open();
  f.hook((name, args, next) => { if (name === 'nativePlaylistSave') { next(name, args); throw new Error('Reply lost'); } });
  f.$('playlist-save-confirm').click(); await settle();
  assert.equal(f.native.adapter.controller.state.uncertain, true);
  f.$('close-playlist-save').click();
  assert.match(f.$('open-playlist-save').textContent, /Check the last save/);
  f.$('open-playlist-save').click();
  const gate = deferred(); f.hook(name => name === 'nativePlaylistCatalog' ? gate.promise : undefined);
  f.$('playlist-save-check-catalog').click(); await settle();
  f.doc.fire('fsonos:catalog-changed', { household: 'S2', playlists: true });
  gate.resolve(structuredClone(f.catalogs.get('Den@S2'))); await settle();
  assert.equal(f.native.adapter.controller.state.uncertain, true);
  assert.equal(f.native.panel.control.state.result.status, 'uncertain');
  assert.notEqual(f.native.panel.control.state.catalogStatus, 'ready'); assert.equal(f.writes().length, 1);
});

test('mounted workspace changes and page hiding close the review without sending a save', async t => {
  const f = mounted(); t.after(() => f.native.dispose());
  f.doc.fire('fsonos:studio-selection', { key: 'original', workspace: 'listen' });
  await f.open();
  f.doc.fire('fsonos:studio-selection', { key: 'original', workspace: 'house' });
  assert.equal(f.$('playlist-save-dialog').open, false); assert.equal(f.native.panel.control.state.pending, null);
  f.$('playlist-save-confirm').click(); await settle(); assert.equal(f.writes().length, 0);
  await f.open();
  f.doc.hidden = true; f.doc.fire('visibilitychange');
  assert.equal(f.$('playlist-save-dialog').open, false); assert.equal(f.native.adapter.controller.state.active, false);
  f.$('playlist-save-confirm').click(); await settle(); assert.equal(f.writes().length, 0);
});

test('explicit phone Read restores an invalidated observation without reviving its old review', async t => {
  const f = mounted(); t.after(() => f.native.dispose());
  await f.open(); const old = f.native.panel.control.state.pending;
  const parts = f.cards.get('Den@S2');
  f.native.adapter.invalidate(parts); f.native.adapter.observe(parts);
  assert.equal(f.native.panel.control.state.pending, null);
  assert.equal(f.native.panel.control.canRead(), false);
  assert.equal(f.$('playlist-save-read').disabled, false);
  f.$('playlist-save-read').click(); await settle();
  assert.equal(f.native.panel.control.state.loaded, true);
  assert.equal(f.native.panel.control.state.pending, null);
  assert.equal(await f.native.panel.control.apply(old), false);
  assert.equal(f.writes().length, 0);
});

test('explicit phone Read refreshes an expired group observation before reading its queue', async t => {
  const f = mounted(); t.after(() => f.native.dispose());
  await f.open(); const old = f.native.panel.control.state.pending, before = f.calls.length;
  f.advance(31000);
  assert.equal(f.native.panel.control.canRead(), false);
  f.$('playlist-save-read').click(); await settle();
  assert.ok(f.calls.length > before);
  assert.equal(f.native.panel.control.state.loaded, true);
  assert.equal(f.native.panel.control.state.title, 'My retained mix');
  assert.equal(f.native.panel.control.state.pending, null);
  assert.equal(await f.native.panel.control.apply(old), false);
  assert.equal(f.writes().length, 0);
  assert.equal(f.calls.at(-1).name, 'queue');
});

test('expired Read preparation cannot adopt a replaced card or reopen a hidden view', async t => {
  for (const kind of ['card', 'hidden']) {
    const f = mounted(); t.after(() => f.native.dispose());
    await f.open(); f.advance(31000); const gate = deferred();
    f.hook(name => name === 'zones' ? gate.promise : undefined);
    f.$('playlist-save-read').click(); await settle();
    if (kind === 'card') {
      const old = f.cards.get('Den@S2'); old.disposed = true;
      f.cards.set('Den@S2', { ...old, disposed: false, writing: false });
      f.native.adapter.invalidate(old);
    } else f.$('close-playlist-save').click();
    gate.resolve(structuredClone(f.zones)); await settle();
    assert.equal(f.native.panel.control.state.loaded, false);
    assert.equal(f.native.panel.control.state.pending, null); assert.equal(f.writes().length, 0);
    if (kind === 'hidden') assert.equal(f.$('playlist-save-dialog').open, false);
  }
});

test('mounted saved-catalog handoff waits for source-draft refusal or async catalog failure', async t => {
  for (const delayed of [false, true]) {
    const f = mounted(); t.after(() => f.native.dispose());
    await f.open(); f.$('playlist-save-confirm').click(); await settle();
    const gate = deferred(), saved = f.native.panel.control.state.result;
    f.doc.addEventListener('fsonos:playlist-browse', event => {
      event.preventDefault(); event.detail.response = delayed ? gate.promise : Promise.resolve(false);
    });
    f.$('playlist-save-browse').click(); await settle();
    assert.equal(f.writes().length, 1);
    if (delayed) { assert.equal(f.$('playlist-save-dialog').open, false); gate.resolve(false); await settle(); }
    assert.equal(f.$('playlist-save-dialog').open, true);
    assert.equal(f.native.panel.control.state.result, saved);
    assert.match(f.$('playlist-save-message').textContent, /source editor could not select/);
    assert.equal(f.writes().length, 1);
  }
});
