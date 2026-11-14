'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const F = require('../crates/fsonos-api/src/remote/favorites.js');
const Panel = require('../crates/fsonos-api/src/remote/favorites-panel.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const tick = () => new Promise(resolve => setImmediate(resolve));
async function until(check) { for (let i = 0; i < 80 && !check(); ++i) await tick(); assert.ok(check(), 'expected view state arrives'); }
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };

function documentFixture() {
  const nodes = [];
  class Element {
    constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map(); this.events = new Map(); this.dataset = {}; this.value = ''; this.open = false; this.isConnected = true; this._text = ''; nodes.push(this); }
    set textContent(value) { this._text = String(value); this.children = []; }
    get textContent() { return this._text + this.children.map(node => node.textContent).join(''); }
    append(...children) { for (const child of children) { child.parentElement = this; this.children.push(child); } }
    replaceChildren(...children) { this._text = ''; this.children = []; this.append(...children); }
    before(node) { this.parentElement?.append(node); }
    setAttribute(key, value) { this.attributes.set(key, String(value)); }
    getAttribute(key) { return this.attributes.get(key) ?? null; }
    addEventListener(key, fn) { const list = this.events.get(key) || []; list.push(fn); this.events.set(key, list); }
    removeEventListener(key, fn) { this.events.set(key, (this.events.get(key) || []).filter(item => item !== fn)); }
    fire(key, detail) { for (const fn of this.events.get(key) || []) fn({ target: this, detail, preventDefault() {} }); }
    dispatchEvent(event) { this.fire(event.type, event.detail); return true; }
    click() { if (!this.disabled) this.fire('click'); }
    focus() { doc.activeElement = this; }
    showModal() { this.open = true; }
    close() { this.open = false; this.fire('close'); }
  }
  const doc = new Element('document');
  Object.assign(doc, { hidden: false, activeElement: null, createElement: tag => new Element(tag),
    getElementById: id => nodes.find(node => node.id === id),
    querySelector: selector => selector === 'dialog[open]' ? nodes.find(node => node.tagName === 'DIALOG' && node.open) || null : null });
  doc.body = new Element('body'); doc.defaultView = new Element('window');
  doc.defaultView.CustomEvent = class { constructor(type, options) { this.type = type; this.detail = options.detail; } };
  const host = new Element('section'); host.id = 'households'; doc.body.append(host);
  const hero = new Element('div'), source = new Element('button'); source.id = 'open-source'; hero.append(source); doc.body.append(hero);
  return { doc, nodes, host, $: id => doc.getElementById(id) };
}

async function fixture(kind, { lost = false, demo = false, logged = true, catalogChanged = () => {} } = {}) {
  const ui = documentFixture(); ui.doc.body.dataset.preview = String(demo);
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'playing' }));
  const catalogs = new Map(zones.map(zone => ['Den@' + zone.household, [
    { id: 'FV:2/7', title: 'Same title', kind: 'stream' }, { id: 'FV:2/8', title: 'Same title', kind: 'track' }] ]));
  const calls = [], cards = new Map(zones.map(zone => [Core.key(zone), { zone, writing: false, disposed: false, observers: [] }]));
  let controller, view, hook, begins = 0, ends = 0;
  const read = url => {
    const p = new URL(url, 'http://fixture'), q = Object.fromEntries(p.searchParams);
    if (p.pathname === '/zones') return structuredClone(zones);
    if (p.pathname.endsWith('/state')) return { zone: zones.find(zone => Core.key(zone) === decodeURIComponent(p.pathname.split('/')[2])), transport_state: 'playing', volume: 20, mute: false, track: null };
    if (p.pathname === '/favorites') return structuredClone(catalogs.get(q.zone));
    if (p.pathname === '/favorites/current') return { zone: 'Den', favorite: { id: '', title: 'Station & <literal>', kind: 'stream', description: 'The original station' }, revision: 'current:1' };
    if (p.pathname === '/favorites/inspect') return { zone: 'Den', favorite: structuredClone(catalogs.get(q.zone).find(item => item.id === q.favorite)), revision: 'favorite:1' };
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    const operation = url.split('/').at(-1), rows = catalogs.get(body.zone);
    let favorite, changed = true;
    if (operation === 'save') { favorite = { id: 'FV:2/9', title: body.title, kind: 'stream' }; rows.push(favorite); }
    else { const index = rows.findIndex(row => row.id === body.favorite); favorite = { ...rows[index] };
      if (operation === 'rename') { changed = body.title !== favorite.title; favorite.title = body.title; rows[index] = favorite; }
      else rows.splice(index, 1);
    }
    if (lost) throw new Error('Reply lost after favorite changed');
    return { done: 'Favorite ' + operation + ' confirmed.', zone: 'Den', favorite, operation, changed, logged };
  };
  const call = async (url, body) => { calls.push({ url, body }); return hook ? hook(url, body, read, write) : body === undefined ? read(url) : write(url, body); };
  if (kind === 'dashboard') {
    controller = Core.model({ call, now: () => 0, demo, changed: () => view?.observe() });
    view = Panel.dashboard({ call, controller, demo, document: ui.doc });
    if (!demo) { await controller.refresh(); controller.connection(true); }
  } else {
    view = Panel.phone({ document: ui.doc, catalogChanged,
      call(name, args = {}) { const [method, route] = F.routes[name]; return call(route + (args.query && Object.keys(args.query).length ? '?' + new URLSearchParams(args.query) : ''), method === 'POST' ? args.body : undefined); },
      cards: () => cards.values(), current: parts => !parts.disposed && cards.get(Core.key(parts.zone)) === parts,
      busy: parts => [...cards.values()].some(other => other.writing && other.zone.household === parts.zone.household && other.zone.members.some(member => parts.zone.members.includes(member))),
      begin(parts) { if (parts.writing) return false; ++begins; parts.writing = true; parts.observers.forEach(observer => observer.observe()); return true; },
      end(parts) { ++ends; parts.writing = false; parts.observers.forEach(observer => observer.observe()); },
      register(make) { for (const parts of cards.values()) { const observer = make(parts); parts.observers.push(observer); ui.host.append(observer.node); } },
    });
  }
  calls.length = 0;
  return { ...ui, view, controller, calls, cards, zones, catalogs,
    hook(value) { hook = value; }, writes: () => calls.filter(row => row.body !== undefined), lanes: () => [begins, ends],
    async open() { view.trigger.click(); await until(() => view.control.state.open && !view.control.state.reading); },
    async save(title = '<img src=x> & Dvořák') {
      ui.$('favorites-current').click(); await until(() => view.control.state.current);
      ui.$('favorites-name').value = title; ui.$('favorites-name').fire('input');
      ui.$('favorites-form').fire('submit'); await until(() => view.control.state.pending);
      ui.$('favorites-confirm').click(); await until(() => view.control.state.result && !view.control.state.writing);
    },
  };
}

for (const kind of ['phone', 'dashboard']) {
  test(`${kind} panel previews the actual station and saves a literal title after explicit confirmation`, async t => {
    const f = await fixture(kind); t.after(() => f.view.dispose());
    assert.equal(f.calls.length, 0); await f.open(); assert.equal(f.$('favorites-dialog').open, true);
    assert.equal(f.$('favorites-target').parentElement.hidden, kind === 'dashboard');
    f.$('favorites-current').click(); await until(() => f.view.control.state.current);
    assert.match(f.$('favorites-source').textContent, /Station & <literal>.*stream/);
    assert.match(f.$('favorites-effect').textContent, /complete queue is not saved/);
    f.$('favorites-name').value = '<img src=x> & Dvořák'; f.$('favorites-name').fire('input');
    f.$('favorites-form').fire('submit'); await until(() => f.view.control.state.pending);
    assert.equal(f.writes().length, 0); assert.equal(f.doc.activeElement, f.$('favorites-preview'));
    assert.match(f.$('favorites-preview-title').textContent, /<img src=x>/);
    f.$('favorites-confirm').click(); await until(() => f.view.control.state.result && !f.view.control.state.writing);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/favorites/save');
    assert.equal(f.$('favorites-result-title').textContent, 'Favorite saved'); assert.equal(f.$('favorites-result-id').textContent, 'FV:2/9');
    assert.equal(f.nodes.some(node => node.tagName === 'IMG'), false, 'native titles are only text');
    if (kind === 'phone') assert.deepEqual(f.lanes(), [1, 1]);
  });

  test(`${kind} panel exact duplicate-title row removal reviews its ID and original household`, async t => {
    const f = await fixture(kind); t.after(() => f.view.dispose()); await f.open();
    const button = f.nodes.find(node => node.getAttribute('aria-label') === 'Remove Same title (FV:2/8)');
    assert.ok(button); button.click(); await until(() => f.view.control.state.pending);
    assert.match(f.$('favorites-preview-source').textContent, /track.*FV:2\/8/);
    assert.match(f.$('favorites-preview-group').textContent, /S1/); assert.equal(f.writes().length, 0);
    f.$('favorites-confirm').click(); await until(() => f.view.control.state.result && !f.view.control.state.writing);
    assert.equal(f.writes()[0].body.favorite, 'FV:2/8'); assert.equal(f.catalogs.get('Den@S1')[0].id, 'FV:2/7');
  });

  test(`${kind} panel keeps a lost reply visible and recovers the original household after reopening`, async t => {
    const f = await fixture(kind, { lost: true }); t.after(() => f.view.dispose()); await f.open(); await f.save('Keep this station');
    assert.equal(f.$('favorites-result-title').textContent, 'Favorite change needs inspection');
    f.$('favorites-close').click(); assert.equal(f.$('favorites-dialog').open, false);
    await f.open(); assert.equal(f.$('favorites-current').disabled, true); assert.equal(f.$('favorites-recover').hidden, false);
    if (f.controller) f.controller.select('Den@S2');
    else { f.$('favorites-target').value = 'Den@S2'; f.$('favorites-target').fire('change'); await until(() => !f.view.control.state.reading); }
    f.$('favorites-recover').click(); await until(() => f.view.control.state.result.status === 'observed');
    assert.equal(f.$('favorites-result-group').textContent, 'Den@S1'); assert.match(f.$('favorites-recovery-catalog').textContent, /Keep this station/);
    assert.equal(f.$('favorites-recover').hidden, true); assert.equal(f.writes().length, 1);
  });

  test(`${kind} panel preserves success and explains an action-log failure`, async t => {
    const f = await fixture(kind, { logged: false }); t.after(() => f.view.dispose()); await f.open(); await f.save();
    assert.equal(f.$('favorites-result-title').textContent, 'Favorite saved');
    assert.match(f.$('favorites-result-detail').textContent, /action log.*Do not repeat/); assert.equal(f.writes().length, 1);
  });

  test(`${kind} design preview opens the editor without touching any API`, async t => {
    const f = await fixture(kind, { demo: true }); t.after(() => f.view.dispose()); await f.open();
    assert.equal(f.calls.length, 0); assert.equal(f.$('favorites-current').disabled, true); assert.equal(f.$('favorites-refresh').disabled, true);
  });
}

test('phone holds the original physical command lane through close and a late native receipt', async t => {
  const f = await fixture('phone'); t.after(() => f.view.dispose()); await f.open();
  f.$('favorites-current').click(); await until(() => f.view.control.state.current);
  f.$('favorites-form').fire('submit'); await until(() => f.view.control.state.pending);
  const gate = deferred(); f.hook(async (url, body, read, write) => { if (body === undefined) return read(url); const result = write(url, body); await gate.promise; return result; });
  f.$('favorites-confirm').click(); await until(() => f.writes().length === 1);
  f.$('favorites-close').click(); assert.deepEqual(f.lanes(), [1, 0]); assert.equal(f.cards.get('Den@S1').writing, true);
  gate.resolve(); await until(() => !f.view.control.state.writing);
  assert.deepEqual(f.lanes(), [1, 1]); assert.equal(f.view.control.state.result.status, 'accepted'); assert.equal(f.writes().length, 1);
});

test('phone card replacement during the final current-source read prevents dispatch and releases the old lane', async t => {
  const f = await fixture('phone'); t.after(() => f.view.dispose()); await f.open();
  f.$('favorites-current').click(); await until(() => f.view.control.state.current);
  f.$('favorites-form').fire('submit'); await until(() => f.view.control.state.pending);
  const gate = deferred(); let observed;
  f.hook((url, body, read, write) => {
    if (body !== undefined) return write(url, body);
    const value = read(url); if (url.startsWith('/favorites/current')) { observed = value; return gate.promise; } return value;
  });
  f.$('favorites-confirm').click(); await until(() => observed);
  const original = f.cards.get('Den@S1'); original.disposed = true;
  f.cards.set('Den@S1', { ...original, writing: false, disposed: false, observers: [] });
  gate.resolve(observed); await until(() => !f.view.control.state.writing);
  assert.equal(f.writes().length, 0); assert.equal(f.view.control.state.result.status, 'not_sent');
  assert.deepEqual(f.lanes(), [1, 1]); assert.equal(original.writing, false);
});

test('dashboard uncertainty cannot be acknowledged through playback or a different catalog', async t => {
  const f = await fixture('dashboard', { lost: true }); t.after(() => f.view.dispose()); await f.open(); await f.save();
  assert.equal(f.controller.state.uncertain, true);
  await f.controller.refresh(true); assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('favorites', F.catalog, { explicit: true, storeTarget: 'Den@S2' }); assert.equal(f.controller.state.uncertain, true);
  await f.view.control.recover(); assert.equal(f.controller.state.uncertain, false);
});

for (const operation of ['save', 'rename', 'remove']) test(`phone ${operation} refreshes the existing Play-a-favorite picker without waiting for a server event`, async t => {
  let legacy, now = 0, timerId = 0;
  const timers = new Map(), reads = [];
  const settle = async () => { for (let i = 0; i < 60; ++i) await Promise.resolve(); };
  const f = await fixture('phone', { catalogChanged: change => legacy.catalogs.invalidate(change) });
  t.after(() => { f.view.dispose(); legacy?.catalogs.dispose(); });
  const source = fs.readFileSync(path.resolve(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  legacy = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\ncall = network; ({ catalogs, house, zoneKey });', {
    document: f.doc, Date: { now: () => now },
    setTimeout(fn, delay) { timers.set(++timerId, { fn, at: now + delay }); return timerId; },
    clearTimeout(id) { timers.delete(id); },
    network: async (route, args) => {
      if (route === 'moods') return { moods: [] };
      assert.equal(route, 'favorites'); reads.push(args.query.zone);
      return structuredClone(f.catalogs.get(args.query.zone));
    },
  });
  for (const parts of f.cards.values()) {
    parts.favorite = f.doc.createElement('select'); parts.mood = f.doc.createElement('select');
    legacy.house.cards.set(legacy.zoneKey(parts.zone), parts);
  }
  legacy.catalogs.reconcile(f.zones); legacy.catalogs.resume(); await settle();
  const original = f.cards.get('Den@S1').favorite, other = f.cards.get('Den@S2').favorite;
  const rows = picker => picker.children.filter(node => node.getAttribute('value')).map(node => [node.getAttribute('value'), node.textContent]);
  assert.deepEqual(rows(original), [['FV:2/7', 'Same title'], ['FV:2/8', 'Same title']]);
  await f.open();
  if (operation === 'save') await f.save('A new station');
  else {
    f.view.control.select(f.view.control.state.catalog.find(item => item.id === 'FV:2/7'));
    if (operation === 'rename') f.view.control.editTitle('Renamed station');
    await f.view.control.confirm(await f.view.control.review(operation));
  }
  assert.equal(f.view.control.state.result.status, 'accepted'); assert.equal(legacy.house.favorites.has('S1'), false, 'local success retires the old picker immediately');
  assert.deepEqual(rows(other), [['FV:2/7', 'Same title'], ['FV:2/8', 'Same title']], 'the other household stays current');
  now = 300;
  for (const [id, timer] of [...timers]) if (timer.at <= now) { timers.delete(id); timer.fn(); }
  await settle();
  assert.equal(reads.filter(target => target === 'Den@S1').length, 2);
  assert.equal(reads.filter(target => target === 'Den@S2').length, 1);
  if (operation === 'save') assert.deepEqual(rows(original).at(-1), ['FV:2/9', 'A new station']);
  else if (operation === 'rename') assert.deepEqual(rows(original)[0], ['FV:2/7', 'Renamed station']);
  else assert.deepEqual(rows(original), [['FV:2/8', 'Same title']]);
  assert.equal(f.writes().length, 1);
});

test('both served bundles include the common favorites model before its panel and parse as one script', () => {
  const root = path.resolve(__dirname, '../crates/fsonos-api/src');
  for (const file of ['http/remote.rs', 'http/remote/dashboard.rs']) {
    const source = fs.readFileSync(path.join(root, file), 'utf8'), parts = new Map();
    for (const match of source.matchAll(/const (\w+): &str = include_str!\("([^"]+)"\);/g)) parts.set(match[1], fs.readFileSync(path.resolve(root, path.dirname(file), match[2]), 'utf8'));
    const chain = [...source.matchAll(/"((?:\{\w+\}\\n)+\{\w+\})"/g)].map(match => match[1]).find(value => value.includes(file === 'http/remote.rs' ? '{EVENT_FEED_SCRIPT}' : '{EVENTS}'));
    assert.ok(chain); const keys = [...chain.matchAll(/\{(\w+)\}/g)].map(match => match[1]);
    const model = file === 'http/remote.rs' ? 'FAVORITES_SCRIPT' : 'FAVORITES';
    const panel = file === 'http/remote.rs' ? 'FAVORITES_PANEL_SCRIPT' : 'FAVORITES_PANEL';
    assert.ok(keys.indexOf(model) > keys.indexOf(file === 'http/remote.rs' ? 'SOURCE_PLAYER_SCRIPT' : 'SOURCE_PLAYER'));
    assert.ok(keys.indexOf(panel) > keys.indexOf(model));
    new vm.Script(keys.map(key => { assert.ok(parts.has(key), key); return parts.get(key); }).join('\n'), { filename: file });
    assert.match(source, /\{FAVORITES_STYLE\}/);
  }
});
