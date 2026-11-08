'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const P = require('../crates/fsonos-api/src/remote/dj-preferences.js');
const Taste = require('../crates/fsonos-api/src/remote/dashboard-preferences.js');
const copy = value => structuredClone(value);
const defaults = () => ({ energy: null, explicit: false,
  favor: { genres: [], artists: [], eras: [], moods: [] }, avoid: { genres: [], artists: [], eras: [], moods: [] },
  pin: { artists: [], albums: [], tracks: [] }, ban: { artists: [], albums: [], tracks: [] } });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
async function settled(predicate) {
  for (let i = 0; i < 200 && !predicate(); i++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached expected boundary');
}
function documentFixture() {
  const roots = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, dataset: {}, value: '', type: '', disabled: false, open: false, isConnected: true,
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      setAttribute(name, value) { this.attributes[name] = String(value); }, getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this._text = ''; this.children = children; },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(node => node.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; } };
  }, getElementById(id) { return roots.get(id) || null; } };
  const html = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/dashboard.html'), 'utf8');
  for (const [, tag, id] of html.matchAll(/<([a-z][a-z0-9]*)\s[^>]*?id="([^"]+)"[^>]*>/g)) roots.set(id, doc.createElement(tag));
  return doc;
}
function fixture({ speakers = false, demo = false, network, dom = false } = {}) {
  const prefs = defaults(), calls = [];
  const zone = { household: 'S2', coordinator_room: 'Synthetic room', members: ['Synthetic room'], transport_state: 'playing' };
  const playback = { zone, transport_state: 'playing', volume: 20, track: { uri: 'fixture:track' } };
  let time = 1000, reader = null, writer = null, taste;
  const read = url => {
    if (url === '/dj/preferences') return copy(prefs);
    if (url === '/schedules') return [];
    if (url === '/zones') return speakers ? [copy(zone)] : [];
    if (url.endsWith('/state')) return copy(playback);
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.equal(url, '/dj/preferences');
    const { key, value, unset } = body;
    if (key === 'energy') prefs.energy = unset ? null : Number(value);
    else if (key === 'explicit') prefs.explicit = !unset && value === 'true';
    else {
      const [group, field] = key.split('.');
      if (unset) prefs[group][field] = prefs[group][field].filter(item => item !== value);
      else if (!prefs[group][field].includes(value)) prefs[group][field].push(value);
    }
    return { done: 'Standing preference saved.', changed: true, preferences: copy(prefs) };
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, options, write) : write(url, body);
    return reader ? reader(url, options, read) : read(url);
  };
  const call = network ? network(raw) : raw;
  const controller = C.model({ call, demo, now: () => time, changed: () => taste?.observe() });
  const doc = dom ? documentFixture() : null;
  const panel = dom ? Taste.panel({ call, controller, document: doc, now: () => time }) : null;
  taste = panel ? panel.control : Taste.model({ call, controller, now: () => time });
  return { prefs, calls, call, controller, taste, playback, doc, panel,
    reader(fn) { reader = fn; }, writer(fn) { writer = fn; }, time(value) { time = value; },
    writes: () => calls.filter(row => row.body !== undefined),
    async ready() { taste.open(); assert.equal(await taste.read(false), true); return this; } };
}

test('standing taste is lazy and works without discovery, a selected room or online speakers', async () => {
  const f = fixture(); f.taste.open(); assert.deepEqual(f.calls, []);
  assert.equal(await f.taste.read(false), true); assert.equal(f.taste.editable(), true);
  assert.equal(f.controller.selected(), undefined); assert.equal(f.controller.state.initialized, false);
  assert.deepEqual(f.calls.map(row => row.url), ['/dj/preferences']);
  for (let i = 0; i < 10; i++) f.taste.observe();
  assert.equal(f.calls.length, 1, 'observing does not poll the daemon');
});

test('all list keys add one literal value through one native request with no playback calls', async () => {
  for (const key of P.listKeys) {
    const f = await fixture().ready();
    const pending = f.taste.review(key, '  Earth, Wind & Fire  ');
    assert.ok(Object.isFrozen(pending) && Object.isFrozen(pending.body));
    assert.equal(f.writes().length, 0); assert.equal(await f.taste.apply(pending), true);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [
      { url: '/dj/preferences', body: { key, value: 'Earth, Wind & Fire', unset: false } },
    ]);
    assert.equal(f.calls.length, 3, 'initial read, preflight read and one write');
    assert.equal(f.controller.state.uncertain, false); assert.equal(f.taste.editable(), true);
    const [group, field] = key.split('.'); assert.deepEqual(f.taste.state.preferences[group][field], ['Earth, Wind & Fire']);
  }
});

test('single-item removal preserves other entries and refuses a whole-list unset', async () => {
  const f = fixture(); f.prefs.ban.artists = ['One', 'Two']; await f.ready();
  assert.equal(f.taste.review('ban.artists', null, true), null);
  assert.equal(f.taste.review('ban.artists', '', true), null);
  const pending = f.taste.review('ban.artists', 'One', true);
  assert.equal(await f.taste.apply(pending), true);
  assert.deepEqual(f.prefs.ban.artists, ['Two']); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.writes()[0].body, { key: 'ban.artists', value: 'One', unset: true });
});

test('energy bounds, time-of-day reset and explicit choices use the existing validated shapes', async () => {
  const f = await fixture().ready();
  for (const [key, value, unset, expected] of [
    ['energy', '0', false, 0], ['energy', '100', false, 100], ['energy', null, true, null],
    ['explicit', 'true', false, true], ['explicit', 'false', false, false], ['explicit', null, true, false],
  ]) {
    const pending = f.taste.review(key, value, unset); assert.ok(pending);
    assert.equal(await f.taste.apply(pending), true); assert.equal(f.taste.state.preferences[key], expected);
  }
  for (const [key, value] of [['energy', '-1'], ['energy', '101'], ['energy', '1e2'], ['energy', '1.5'],
    ['explicit', 'yes'], ['favor.genres', ' '], ['ban.tracks', 'x'.repeat(257)], ['unknown', 'anything']]) {
    assert.equal(f.taste.review(key, value), null);
  }
  assert.equal(f.writes().length, 6);
});

test('the preflight preserves changes to unrelated preferences without submitting an old snapshot', async () => {
  const f = await fixture().ready(), pending = f.taste.review('favor.genres', 'jazz');
  f.prefs.ban.artists = ['Concurrent artist']; f.prefs.energy = 0;
  assert.equal(await f.taste.apply(pending), true);
  assert.deepEqual(f.taste.state.preferences.ban.artists, ['Concurrent artist']);
  assert.equal(f.taste.state.preferences.energy, 0);
  assert.deepEqual(Object.keys(f.writes()[0].body), ['key', 'value', 'unset']);
});

test('a changed reviewed key or scalar stops before sending and requires another review', async () => {
  for (const key of ['favor.artists', 'energy']) {
    const f = await fixture().ready(), pending = f.taste.review(key, key === 'energy' ? '80' : 'Reviewed artist');
    if (key === 'energy') f.prefs.energy = 10; else f.prefs.favor.artists = ['Added elsewhere'];
    assert.equal(await f.taste.apply(pending), false); assert.equal(f.writes().length, 0);
    assert.equal(f.controller.state.uncertain, false); assert.equal(f.taste.editable(), false);
    assert.match(f.taste.state.message, /changed since you reviewed/);
    assert.equal(await f.taste.read(), true); assert.equal(await f.taste.apply(pending), false);
  }
});

test('new drafts, refreshes, cancellations and replacement reviews retire old confirmations', async () => {
  const f = await fixture().ready(), stale = [];
  stale.push(f.taste.review('energy', '40')); f.taste.edit('energy', '50');
  stale.push(f.taste.review('energy', '40')); f.taste.cancel();
  stale.push(f.taste.review('energy', '40')); await f.taste.read();
  stale.push(f.taste.review('energy', '40')); const current = f.taste.review('explicit', 'true');
  for (const pending of stale) assert.equal(await f.taste.apply(pending), false);
  assert.equal(await f.taste.apply({ ...current }), false);
  assert.equal(await f.taste.apply(current), true); assert.equal(await f.taste.apply(current), false);
  assert.equal(f.writes().length, 1);
});

test('expired observations disable edits without polling or silently adopting new settings', async () => {
  const f = await fixture().ready(), pending = f.taste.review('energy', '40');
  f.time(1000 + Taste.MAX_AGE); f.taste.observe();
  assert.equal(f.taste.state.loaded, false); assert.equal(f.taste.editable(), false);
  assert.equal(await f.taste.apply(pending), false); assert.equal(f.calls.length, 1);
  assert.ok(f.taste.state.preferences); assert.match(f.taste.state.message, /Refresh preferences/);
  await f.taste.read(); assert.equal(f.taste.editable(), true);
});

test('failed and malformed reads preserve the last observation without inventing editable defaults', async () => {
  const f = fixture(); f.prefs.energy = 55; await f.ready();
  f.reader(() => { throw new Error('Stored file unavailable'); });
  assert.equal(await f.taste.read(), false); assert.equal(f.taste.state.preferences.energy, 55); assert.equal(f.taste.editable(), false);
  f.reader(() => ({ explicit: false })); assert.equal(await f.taste.read(), false);
  assert.equal(f.taste.state.preferences.energy, 55); assert.equal(f.taste.state.pending, null); assert.equal(f.writes().length, 0);
});

test('closing or hiding during preflight aborts the read and prevents an unsent preference write', async () => {
  for (const action of ['close', 'suspend', 'dispose']) {
    const f = await fixture().ready(), wait = deferred(), pending = f.taste.review('energy', '70');
    let signal; f.reader((url, options) => { signal = options.signal; return wait.promise; });
    const saving = f.taste.apply(pending); await settled(() => signal);
    f.taste[action](); assert.equal(signal.aborted, true); wait.resolve(defaults());
    assert.equal(await saving, false); assert.equal(f.writes().length, 0); assert.equal(f.controller.state.busy, false);
  }
});

test('a stale read cannot overwrite a newer observation after close and reopen', async () => {
  const f = fixture(), wait = deferred(); let first = true;
  f.reader((url, options, read) => { if (first) { first = false; return wait.promise; } return read(url); });
  f.taste.open(); const old = f.taste.read(); f.taste.close(); f.prefs.energy = 90;
  f.taste.open(); assert.equal(await f.taste.read(), true);
  wait.resolve({ ...defaults(), energy: 10 }); assert.equal(await old, false);
  assert.equal(f.taste.state.preferences.energy, 90);
});

test('double clicks and competing dashboard commands share one mutation lane', async () => {
  const f = await fixture({ speakers: true }).ready(); await f.controller.refresh();
  const pending = f.taste.review('energy', '50'), wait = deferred();
  f.writer(() => wait.promise); const saving = f.taste.apply(pending);
  await settled(() => f.writes().length === 1);
  assert.equal(f.controller.state.busy, true); assert.equal(await f.taste.apply(pending), false);
  assert.equal(await f.controller.command('pause'), false);
  assert.equal(await f.controller.mutate(() => { throw new Error('Must not start'); }, { stored: 'schedules' }), false);
  assert.equal(await f.taste.read(), false); assert.equal(f.taste.review('energy', '60'), null);
  wait.resolve({ done: 'Saved.', changed: true, preferences: { ...defaults(), energy: 50 } });
  assert.equal(await saving, true); assert.equal(f.writes().length, 1); assert.equal(f.controller.state.busy, false);
});

test('an already submitted save survives closing and hiding with its receipt and no replay', async () => {
  const f = await fixture().ready(), pending = f.taste.review('energy', '50'), wait = deferred();
  f.writer(() => wait.promise); const saving = f.taste.apply(pending);
  await settled(() => f.writes().length === 1); f.taste.suspend(); f.controller.suspend();
  wait.resolve({ done: 'Saved while hidden.', changed: true, preferences: { ...defaults(), energy: 50 } });
  assert.equal(await saving, true); assert.equal(f.taste.state.result.done, 'Saved while hidden.');
  assert.equal(f.taste.state.loaded, false); assert.equal(f.writes().length, 1);
  f.controller.resume(); f.taste.resume(); f.taste.open(); assert.equal(f.taste.editable(), false);
  assert.equal(await f.taste.apply(pending), false); assert.equal(f.calls.length, 3);
});

test('a lost preference reply locks all commands until an explicit matching read; unrelated reads never unlock it', async () => {
  const f = await fixture({ speakers: true }).ready(); await f.controller.refresh();
  f.writer((url, body, options, write) => { write(url, body); throw new Error('Connection lost after saving'); });
  const pending = f.taste.review('energy', '50'); assert.equal(await f.taste.apply(pending), false);
  assert.equal(f.controller.state.uncertain, true); assert.match(f.taste.state.message, /Refresh preferences/);
  assert.equal(f.prefs.energy, 50); assert.equal(f.writes().length, 1);
  await f.controller.refresh(true); assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('schedules', value => value, { explicit: true });
  assert.equal(f.controller.state.uncertain, true);
  await f.taste.read(false); assert.equal(f.taste.state.preferences.energy, 50); assert.equal(f.controller.state.uncertain, true);
  assert.equal(await f.controller.command('pause'), false);
  await f.taste.read(true); assert.equal(f.controller.state.uncertain, false); assert.equal(f.taste.editable(), true);
  assert.equal(await f.taste.apply(pending), false); assert.equal(f.writes().length, 1);
});

test('preference reads cannot acknowledge an uncertain speaker or routine write', async () => {
  for (const stored of [undefined, 'schedules']) {
    const f = await fixture({ speakers: true }).ready(); await f.controller.refresh();
    f.writer(() => { throw new Error('Lost reply'); });
    assert.equal(await f.controller.mutate(({ send }) => send(stored ? '/schedules' : '/pause', stored ? {} : { zone: C.key(f.playback.zone) }),
      stored ? { stored } : {}), false);
    assert.equal(f.controller.state.uncertain, true);
    await f.taste.read(true); assert.equal(f.controller.state.uncertain, true); assert.equal(f.taste.editable(), false);
  }
});

test('malformed acknowledgements and policy failures do not create successful preference state or retry', async () => {
  for (const answer of [null, {}, { done: 'Saved', changed: true }, { done: '', changed: true, preferences: defaults() },
    { done: 'Saved', changed: 'true', preferences: defaults() }, { done: 'Saved', changed: true, preferences: {} }]) {
    const f = await fixture().ready(); f.writer(() => answer);
    assert.equal(await f.taste.apply(f.taste.review('explicit', 'true')), false);
    assert.equal(f.controller.state.uncertain, true); assert.equal(f.taste.state.result, null);
    assert.equal(f.taste.editable(), false); assert.equal(f.writes().length, 1);
  }
  const f = await fixture().ready(); f.writer(() => { throw Object.assign(new Error('Permission denied.'), { hint: 'Ask the owner to grant dj_prefer.' }); });
  assert.equal(await f.taste.apply(f.taste.review('explicit', 'true')), false);
  assert.match(f.taste.state.message, /Ask the owner to grant dj_prefer/); assert.equal(f.writes().length, 1);
});

test('the preferences storage lane cannot send speaker, routine, PUT or empty-body mutations', async () => {
  const cases = [['/play', {}, undefined], ['/schedules', {}, undefined], ['/dj/preferences', {}, { method: 'PUT' }],
    ['/dj/preferences', undefined, undefined], ['/dj/preferences?key=energy', {}, undefined]];
  for (const [url, body, options] of cases) {
    const f = await fixture().ready();
    assert.equal(await f.controller.mutate(({ send }) => send(url, body, options), { stored: 'preferences' }), false);
    assert.equal(f.writes().length, 0); assert.equal(f.controller.state.uncertain, false);
  }
});

test('preview exposes observed taste but can never review or dispatch a mutation', async () => {
  const f = await fixture({ demo: true }).ready();
  assert.ok(f.taste.state.preferences); assert.equal(f.taste.editable(), false);
  assert.equal(f.taste.review('energy', '40'), null); assert.equal(await f.taste.apply(), false); assert.equal(f.writes().length, 0);
});

test('actual panel handlers read, review, save and remove a literal artist without changing music', async () => {
  const f = fixture({ dom: true }), get = id => f.doc.getElementById(id);
  assert.equal(f.calls.length, 0); assert.equal(get('preferences-key').children.length, 14);
  get('open-preferences').fire('click'); await settled(() => f.taste.state.loaded);
  assert.equal(get('preferences-dialog').open, true); assert.match(get('preferences-freshness').textContent, /every group/);
  get('preferences-key').value = 'favor.artists'; get('preferences-key').fire('change');
  get('preferences-value').value = 'Earth, Wind & Fire'; get('preferences-value').fire('input');
  get('preferences-form').fire('submit'); assert.equal(f.writes().length, 0);
  assert.match(get('preferences-preview-title').textContent, /Earth, Wind & Fire/);
  assert.equal(f.doc.activeElement, get('preferences-preview'));
  get('preferences-confirm').fire('click'); await settled(() => f.taste.state.result);
  assert.deepEqual(f.writes()[0].body, { key: 'favor.artists', value: 'Earth, Wind & Fire', unset: false });
  assert.match(get('preferences-result').textContent, /queued music keeps playing/);
  assert.match(get('preferences-lists').textContent, /Earth, Wind & Fire/);
  const remove = get('preferences-lists').querySelectorAll('[data-preference-item]')[0];
  remove.fire('click'); assert.equal(f.taste.state.pending.body.unset, true);
  get('preferences-confirm').fire('click'); await settled(() => f.writes().length === 2 && !f.taste.state.writing);
  assert.deepEqual(f.prefs.favor.artists, []);
  get('preferences-dialog').fire('cancel'); assert.equal(get('preferences-dialog').open, false);
  assert.equal(f.doc.activeElement, get('open-preferences')); assert.equal(f.taste.state.open, false);
});

test('the rendered panel preserves literal metadata, permits scalar reset and retires edited reviews', async () => {
  const f = fixture({ dom: true }), get = id => f.doc.getElementById(id);
  f.prefs.ban.artists = ['<img src=x onerror="throw 1">']; await f.ready();
  assert.match(get('preferences-lists').textContent, /<img src=x/);
  get('preferences-energy').value = '0'; get('preferences-energy').fire('input'); get('preferences-energy-save').fire('click');
  const old = f.taste.state.pending;
  get('preferences-energy').value = '1'; get('preferences-energy').fire('input');
  assert.equal(f.taste.state.pending, null); assert.equal(get('preferences-preview').hidden, true);
  assert.equal(await f.taste.apply(old), false);
  get('preferences-energy-reset').fire('click'); get('preferences-confirm').fire('click');
  await settled(() => f.taste.state.result); assert.deepEqual(f.writes()[0].body, { key: 'energy', unset: true });
  get('preferences-explicit').value = 'true'; get('preferences-explicit').fire('change'); get('preferences-explicit-save').fire('click');
  get('preferences-confirm').fire('click'); await settled(() => f.writes().length === 2 && !f.taste.state.writing);
  assert.equal(f.taste.state.preferences.explicit, true);
  f.panel.suspend(); assert.equal(get('preferences-dialog').open, false); assert.equal(f.taste.state.loaded, false);
});

test('real loopback HTTP carries the reviewed literal value once, with JSON and no playback request', async t => {
  const received = [], current = defaults();
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : undefined; received.push({ method: req.method, url: req.url, body, headers: req.headers });
    res.setHeader('Content-Type', 'application/json');
    if (req.url !== '/dj/preferences') { res.statusCode = 404; res.end('{}'); return; }
    if (req.method === 'GET') { res.end(JSON.stringify(current)); return; }
    assert.equal(req.method, 'POST'); current.favor.artists.push(body.value);
    res.end(JSON.stringify({ done: 'Saved.', changed: true, preferences: current }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = C.request({ fetcher: (url, init) => fetch(base + url, init) });
  const controller = C.model({ call }); const taste = Taste.model({ call, controller });
  taste.open(); assert.equal(await taste.read(false), true);
  assert.equal(await taste.apply(taste.review('favor.artists', 'Earth, Wind & Fire')), true);
  assert.deepEqual(received.map(({ method, url }) => [method, url]), [['GET', '/dj/preferences'], ['GET', '/dj/preferences'], ['POST', '/dj/preferences']]);
  assert.deepEqual(received[2].body, { key: 'favor.artists', value: 'Earth, Wind & Fire', unset: false });
  assert.equal(received[2].headers['content-type'], 'application/json');
  assert.deepEqual(taste.state.preferences.favor.artists, ['Earth, Wind & Fire']);
});

test('production and preview bundle the shared parsers before the taste panel and entry point', () => {
  const root = path.join(__dirname, '..');
  const bundle = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote/dashboard.rs'), 'utf8');
  const preview = fs.readFileSync(path.join(root, 'scripts/dashboard-preview.py'), 'utf8');
  for (const name of ['dj-preferences.js', 'dashboard-preferences.js', 'dashboard-preferences.css']) {
    assert.ok(bundle.includes(name)); assert.ok(preview.includes(name));
  }
  assert.ok(bundle.indexOf('{DJ_PREFERENCES}') < bundle.indexOf('{PREFERENCES}'));
  assert.ok(bundle.indexOf('{PREFERENCES}') < bundle.indexOf('{SCRIPT}'));
  const html = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/dashboard.html'), 'utf8');
  assert.match(html, /id="open-preferences"/); assert.match(html, /id="preferences-dialog"/);
  assert.match(html, /Standing preferences · every group/);
  const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/dashboard-preferences.js'), 'utf8');
  assert.doesNotMatch(source, /innerHTML|outerHTML|insertAdjacentHTML|eval\(|setInterval|EventSource/);
});
