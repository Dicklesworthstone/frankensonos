'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const D = require('../crates/fsonos-api/src/remote/dj-direction.js');
const S = require('../crates/fsonos-api/src/remote/dj-steering.js');
const Direction = require('../crates/fsonos-api/src/remote/dashboard-direction.js');
const Rehearsal = require('../crates/fsonos-api/src/remote/dashboard-rehearsal.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 300 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'expected asynchronous boundary was reached');
};
const steering = () => ({ source: 'session', mood: 'calm', session_mood: 'calm',
  constraints: { include_genres: ['jazz'], exclude_artists: ['Synthetic avoid'], energy_bias: -1 },
  added: { exclude_artists: ['Synthetic avoid'] }, expires_at: 2000000000, expires_in_secs: 7200 });
const work = title => ({ composer: 'Synthetic artist', title, movements: 1, minutes: 5,
  reason: { summary: 'Synthetic variety', factors: [{ factor: 'energy_fit', weight: 1200 }], relaxed: [] } });

function documentFixture() {
  const roots = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', type: '', hidden: false, disabled: false, open: false,
      get options() { return this.children.filter(child => child.tagName === 'OPTION'); },
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      setAttribute(name, value) { this.attributes[name] = String(value); if (['id', 'type'].includes(name)) this[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, prepend(...children) { this.children.unshift(...children); },
      replaceChildren(...children) { this._text = ''; this.children = children; },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(node => node.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; }, setSelectionRange(start) { this.selectionStart = start; } };
  }, getElementById(id) { return roots.get(id) || [...roots.values()].flatMap(descendants).find(node => node.id === id) || null; } };
  for (const id of ['direction-dialog', 'open-direction', 'close-direction', 'direction-target', 'direction-read', 'direction-current',
    'direction-effective', 'direction-added', 'direction-deadline', 'direction-problem', 'direction-form', 'direction-mood', 'direction-mood-help',
    'direction-expiry', 'direction-minutes', 'direction-keep', 'direction-duration-field', 'direction-filters', 'direction-review',
    'direction-start', 'direction-clear', 'direction-preview', 'direction-preview-title', 'direction-preview-settings',
    'direction-preview-expiry', 'direction-preview-effect', 'direction-confirm', 'direction-back', 'direction-message']) {
    roots.set(id, doc.createElement(id === 'direction-mood' ? 'select' : 'div'));
  }
  return doc;
}
function fixture({ demo = false, dom = false, network } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Studio',
    members: household === 'S2' ? ['Studio', 'Kitchen'] : ['Studio'], transport_state: 'playing' }));
  const states = new Map(zones.map(zone => [C.key(zone), { zone, transport_state: 'playing', volume: 25,
    track: { uri: 'fixture:track', title: 'Current song', queue_position: 2, position_secs: 30, duration_secs: 300 } }]));
  const statuses = new Map(zones.map(zone => [C.key(zone), { zone: zone.coordinator_room, running: true,
    now: work('Current song'), next: [work('Later song')], steering: steering(), problem: '' }]));
  const moods = { moods: [{ name: 'calm', constraints: { include_genres: ['jazz'], energy_bias: -1 } },
    { name: 'bright', constraints: { energy_bias: 1 } }] };
  const calls = [], updates = [];
  let reader = null, writer = null, now = 1000, control, updater = null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url === '/dj/moods') return copy(moods);
    if (url === '/schedules') return [];
    const found = /^\/zones\/([^/]+)\/(state|dj)$/.exec(url);
    if (found) return copy((found[2] === 'state' ? states : statuses).get(decodeURIComponent(found[1])));
    throw new Error('Unexpected read ' + url);
  };
  const normalWrite = (url, body) => {
    if (!['/dj/steer', '/dj/start'].includes(url)) return { done: 'Other accepted command', changed: true };
    const status = statuses.get(body.zone);
    if (body.clear) status.steering = { source: 'program', mood: 'calm', session_mood: null, constraints: copy(moods.moods[0].constraints), added: {}, expires_at: null, expires_in_secs: null };
    else {
      const base = moods.moods.find(mood => mood.name === body.mood)?.constraints || {};
      status.steering = { source: 'session', mood: body.mood ?? 'calm', session_mood: body.mood ?? null, constraints: { ...copy(base), ...copy(body.constraints) },
        added: copy(body.constraints), expires_at: body.for_secs == null ? null : 1900000000 + body.for_secs, expires_in_secs: body.for_secs ?? null };
    }
    if (url === '/dj/start') { status.running = true; states.get(body.zone).transport_state = 'playing'; }
    return { done: url === '/dj/start' ? 'DJ started with settings' : 'Direction saved', changed: true, notes: [] };
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body, options });
    return body === undefined ? reader ? reader(url, options, normalRead) : normalRead(url)
      : writer ? writer(url, body, normalWrite) : normalWrite(url, body);
  };
  const call = network ? network(raw) : raw;
  const ctl = C.model({ call, demo, now: () => now, changed: () => control?.observe() });
  const doc = dom ? documentFixture() : null, args = { call, controller: ctl, now: () => now,
    updated: async () => { updates.push(true); if (updater) await updater(); } };
  control = dom ? Direction.panel({ ...args, document: doc }) : Direction.model(args);
  return { ctl, control, doc, calls, call, zones, states, statuses, moods, updates, normalRead,
    writes: () => calls.filter(item => item.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { now = value; }, updated(fn) { updater = fn; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; },
    async open() { assert.equal(await control.open(), true); return this; } };
}
const getStatus = f => f.statuses.get(f.ctl.state.selected);
const getState = f => f.states.get(f.ctl.state.selected);
const review = (f, action = 'save') => { const value = f.control.review(action); assert.ok(value, f.control.state.message); return value; };
const rehearsalSettings = () => ({ mood: 'bright', constraints: { include_artists: ['Earth, Wind & Fire'],
  exclude_keywords: ['Live, at home'], max_work_minutes: 45, energy_bias: -1, allow_long_works: true, strict: true } });
const rehearsalFor = (f, settings = rehearsalSettings()) => ({ target: f.ctl.state.selected,
  signature: C.shape(f.ctl.selected().zone), settings });

for (const household of ['S1', 'S2']) test(`rehearsal import reads the exact ${household} group and requires an explicit lifetime without writing`, async () => {
  const f = await fixture().ready(); f.ctl.select('Studio@' + household);
  const before = f.calls.length, observed = copy(getStatus(f));
  assert.equal(await f.control.importRehearsal(rehearsalFor(f)), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url).sort(), ['/dj/moods', '/zones', `/zones/Studio%40${household}/dj`]);
  assert.deepEqual(getStatus(f), observed, 'importing does not change the observed native steering');
  assert.equal(f.control.state.draft.mood, 'bright');
  assert.equal(f.control.state.draft.direction.include_artists, 'Earth, Wind & Fire');
  assert.equal(f.control.state.draft.direction.exclude_keywords, 'Live, at home');
  assert.equal(f.control.state.draft.direction.strict, true);
  assert.equal(f.control.state.draft.direction.exclude_artists, '', 'prior live extras are replaced only in the draft');
  assert.equal(f.control.state.draft.expiry, 'choose'); assert.equal(f.control.state.draft.minutes, '');
  assert.equal(f.control.state.draft.direction.minutes, ''); assert.equal(f.control.state.dirty, true);
  assert.equal(f.control.review('save'), null); assert.match(f.control.state.message, /Choose how long/);
  assert.equal(await f.control.apply(), false); assert.deepEqual(f.writes(), []);
});

test('rehearsal import rejects commands, timing and unsupported fields before reads or draft replacement', async () => {
  const f = await (await fixture().ready()).open();
  f.control.editDirection('include_genres', 'Keep my jazz draft'); const draft = f.control.state.draft;
  const before = f.calls.length, good = rehearsalFor(f);
  for (const incoming of [null, { ...good, seed: 5 }, { ...good, at: '23:00' }, { ...good, works: 12 },
    { ...good, settings: { ...good.settings, zone: 'Elsewhere@S2' } },
    { ...good, settings: { ...good.settings, for_secs: 3600 } },
    { ...good, settings: { ...good.settings, clear: true } },
    { ...good, settings: { constraints: {} } },
    { ...good, settings: { constraints: { include_artists: ['One\nTwo'] } } }]) {
    assert.equal(await f.control.importRehearsal(incoming), false);
  }
  assert.equal(f.calls.length, before); assert.equal(f.control.state.draft, draft);
  assert.deepEqual(f.writes(), []);
});

test('same-name rooms and changed membership cannot redirect rehearsal settings', async () => {
  const f = await fixture().ready(), intended = rehearsalFor(f);
  f.ctl.select('Studio@S1'); const before = f.calls.length;
  assert.equal(await f.control.importRehearsal(intended), false);
  assert.equal(f.ctl.state.selected, 'Studio@S1'); assert.equal(f.calls.length, before);
  f.ctl.select('Studio@S2'); f.zones[0].members.push('New member');
  assert.equal(await f.control.importRehearsal(intended), false);
  assert.equal(f.control.state.imported, null); assert.equal(f.control.canReview('save'), false);
  assert.match(f.control.state.message, /group changed/); assert.deepEqual(f.writes(), []);
});

test('a removed rehearsed mood cannot silently import automatic or different steering', async () => {
  const f = await fixture().ready(); f.moods.moods = f.moods.moods.filter(mood => mood.name !== 'bright');
  assert.equal(await f.control.importRehearsal(rehearsalFor(f)), false);
  assert.equal(f.control.state.imported, null); assert.equal(f.control.state.draft, null);
  assert.match(f.control.state.message, /rehearsed mood is no longer available/); assert.deepEqual(f.writes(), []);
});

test('rehearsal imports cannot overwrite an existing edit or reviewed direction', async () => {
  for (const mode of ['edit', 'review']) {
    const f = await (await fixture().ready()).open();
    if (mode === 'edit') f.control.editDirection('include_artists', 'My unfinished direction'); else review(f);
    const draft = f.control.state.draft, pending = f.control.state.pending, before = f.calls.length;
    assert.equal(await f.control.importRehearsal(rehearsalFor(f)), false);
    assert.equal(f.control.state.draft, draft); assert.equal(f.control.state.pending, pending);
    assert.equal(f.calls.length, before); assert.match(f.control.state.message, /unfinished draft or review/);
  }
});

test('rehearsal settings are copied before asynchronous reads and kept during ordinary rereads', async () => {
  const f = await fixture().ready(), gate = deferred(), incoming = rehearsalFor(f);
  f.read((url, options, read) => url.endsWith('/dj') ? gate.promise : read(url));
  const loading = f.control.importRehearsal(incoming);
  incoming.settings.constraints.include_artists[0] = 'Changed after the click'; incoming.settings.mood = 'calm';
  gate.resolve(f.normalRead('/zones/Studio%40S2/dj')); assert.equal(await loading, true);
  assert.equal(f.control.state.draft.mood, 'bright');
  assert.equal(f.control.state.draft.direction.include_artists, 'Earth, Wind & Fire');
  assert.equal(Object.isFrozen(f.control.state.imported.constraints.include_artists), true);
  f.read(null); const imported = f.control.state.imported, draft = f.control.state.draft;
  f.ctl.invalidate(['Studio@S2']); await f.ctl.refresh(); assert.equal(await f.control.read(), true);
  assert.equal(f.control.state.imported, imported); assert.equal(f.control.state.draft, draft);
  assert.equal(f.control.state.draft.expiry, 'choose'); assert.deepEqual(f.writes(), []);
});

test('a later rehearsal import supersedes an unfinished read without reviving the old draft', async () => {
  const f = await fixture().ready(), gate = deferred(); let first = true, signal;
  f.read((url, options, read) => {
    if (url.endsWith('/dj') && first) { first = false; signal = options.signal; return gate.promise; }
    return read(url);
  });
  const old = f.control.importRehearsal(rehearsalFor(f));
  assert.equal(await f.control.importRehearsal(rehearsalFor(f, { mood: 'calm', constraints: { include_genres: ['jazz'] } })), true);
  assert.equal(signal.aborted, true); gate.resolve(f.normalRead('/zones/Studio%40S2/dj'));
  assert.equal(await old, false); assert.equal(f.control.state.draft.mood, 'calm');
  assert.equal(f.control.state.draft.direction.include_genres, 'jazz');
  assert.equal(f.control.state.draft.direction.include_artists, ''); assert.deepEqual(f.writes(), []);
});

for (const action of ['close', 'suspend', 'dispose', 'select']) test(`${action} prevents a late rehearsal import`, async () => {
  const f = await fixture().ready(), gate = deferred();
  f.read((url, options, read) => url.endsWith('/dj') ? gate.promise : read(url));
  const pending = f.control.importRehearsal(rehearsalFor(f));
  if (action === 'select') f.ctl.select('Studio@S1'); else f.control[action]();
  gate.resolve(f.normalRead('/zones/Studio%40S2/dj'));
  assert.equal(await pending, false); assert.equal(f.control.state.imported, null);
  assert.equal(f.control.state.draft, null); assert.deepEqual(f.writes(), []);
});

test('filter-only rehearsals keep automatic mood and preserve a deadline only when explicitly chosen', async () => {
  const f = await fixture().ready();
  assert.equal(await f.control.importRehearsal(rehearsalFor(f, { constraints: { include_genres: ['jazz'], strict: true } })), true);
  assert.equal(f.control.state.draft.mood, '', 'do not pin the currently effective or saved mood');
  assert.equal(f.control.state.draft.expiry, 'choose');
  f.control.edit('expiry', 'keep'); const pending = review(f);
  getStatus(f).steering.expires_in_secs = 6117;
  assert.equal(await f.control.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/dj/steer', body: { zone: 'Studio@S2', constraints: { include_genres: ['jazz'], strict: true }, for_secs: 6117 } }]);
  assert.equal(f.control.state.imported, null);
});

test('the imported dialog explains its unsaved scope and focuses a required lifetime before review', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  get('open-direction').focus(); assert.equal(await f.control.importRehearsal(rehearsalFor(f)), true);
  assert.equal(get('direction-dialog').open, true); assert.equal(get('direction-rehearsal-note').hidden, false);
  assert.match(get('direction-rehearsal-note').textContent, /exact set, seed, start time, length/);
  assert.equal(get('direction-expiry').value, 'choose'); assert.equal(f.doc.activeElement, get('direction-expiry'));
  assert.equal(get('direction-review').disabled, true); assert.equal(get('direction-confirm').disabled, true);
  get('direction-expiry').value = 'until'; get('direction-expiry').fire('change');
  assert.equal(get('direction-review').disabled, false); assert.equal(get('direction-start').disabled, true);
  get('direction-review').fire('click'); assert.deepEqual(f.writes(), []);
  assert.match(get('direction-preview-settings').textContent, /Earth, Wind & Fire/);
  assert.equal(get('direction-preview-expiry').textContent, 'Until cleared.');
  get('direction-confirm').fire('click'); await settled(() => !f.control.state.writing && f.writes().length === 1);
  assert.equal(f.writes()[0].url, '/dj/steer'); assert.equal(f.writes()[0].body.for_secs, undefined);
});

test('direction reads on demand and imports only session extras instead of copying mood filters', async () => {
  const f = await fixture().ready(); assert.equal(f.calls.some(item => item.url.endsWith('/dj')), false);
  await f.open();
  assert.equal(f.control.state.target, 'Studio@S2');
  assert.equal(f.control.state.draft.mood, 'calm'); assert.equal(f.control.state.draft.direction.include_genres, '');
  assert.equal(f.control.state.draft.direction.exclude_artists, 'Synthetic avoid'); assert.equal(f.control.state.draft.expiry, 'keep');
  assert.equal(f.writes().length, 0);
});


test('automatic session mood remains inherited instead of pinning its effective mood', async () => {
  const f = await fixture().ready(); getStatus(f).steering.session_mood = null; await f.open();
  assert.equal(f.control.state.status.steering.mood, 'calm');
  assert.equal(f.control.state.draft.mood, '');
  f.control.editDirection('include_artists', 'Synthetic artist');
  const pending = review(f); assert.equal(Object.hasOwn(pending.body, 'mood'), false);
  assert.equal(await f.control.apply(pending), true);
  assert.equal(Object.hasOwn(f.writes()[0].body, 'mood'), false);
  assert.equal(f.control.state.draft.mood, '');
});

test('legacy effective moods require an explicit program or named-mood choice before replacement', async () => {
  for (const chosen of ['', 'calm']) {
    const f = await fixture().ready(); delete getStatus(f).steering.session_mood; await f.open();
    assert.equal(f.control.state.draft.mood, null); assert.equal(f.control.canReview('save'), false);
    assert.equal(f.control.review('save'), null); assert.match(f.control.state.message, /Choose whether.*effective mood/);
    assert.equal(f.control.canReview('clear'), true);
    f.control.edit('mood', chosen);
    const pending = review(f); assert.equal(pending.body.mood, chosen || undefined);
    assert.equal(await f.control.apply(), true); assert.equal(f.writes().length, 1);
  }
});

test('an explicit pin changing to program-following is detected even when the effective mood is identical', async () => {
  const f = await (await fixture().ready()).open(); const pending = review(f);
  getStatus(f).steering.session_mood = null;
  assert.equal(await f.control.apply(pending), false); assert.equal(f.writes().length, 0);
  assert.equal(f.ctl.state.uncertain, false);
});

test('session mood evidence is validated and included in steering identity', () => {
  const known = S.steeringOf({ ...steering(), session_mood: null });
  const legacy = steering(); delete legacy.session_mood;
  assert.equal(S.sessionMoodOf(known), '');
  assert.equal(S.sessionMoodOf(S.steeringOf(steering())), 'calm');
  assert.equal(S.sessionMoodOf(S.steeringOf(legacy)), null);
  assert.equal(S.sessionMoodOf(S.steeringOf({ source: 'session' })), '');
  assert.equal(S.signatureOf(known) === S.signatureOf(S.steeringOf(legacy)), false);
  for (const bad of [{ ...steering(), session_mood: true }, { ...steering(), session_mood: '' },
    { ...steering(), session_mood: 'bright' }, { ...steering(), session_mood: undefined },
    { source: 'program', mood: 'calm', session_mood: 'calm' }]) assert.throws(() => S.steeringOf(bad));
});

test('actual dialog explains ambiguous legacy mood and does not confuse a mood named null with automatic', async () => {
  const f = await fixture({ dom: true }).ready();
  delete getStatus(f).steering.session_mood; f.moods.moods.push({ name: 'null', constraints: {} }); await f.open();
  const mood = f.doc.getElementById('direction-mood'), help = f.doc.getElementById('direction-mood-help');
  assert.equal(mood.value, ''); assert.match(help.textContent, /reports only the effective mood/);
  assert.equal(f.doc.getElementById('direction-review').disabled, true);
  mood.value = JSON.stringify('null'); mood.fire('change');
  assert.equal(f.control.state.draft.mood, 'null'); assert.match(help.textContent, /stays pinned/);
  assert.equal(review(f).body.mood, 'null');
  mood.value = 'null'; mood.fire('change');
  assert.equal(f.control.state.draft.mood, ''); assert.match(help.textContent, /time-of-day mood as it changes/);
  assert.equal(review(f).body.mood, undefined);
});

test('program settings remain inherited when starting a fresh draft', async () => {
  const f = await fixture().ready(); getStatus(f).steering = { source: 'program', mood: 'calm', constraints: { include_genres: ['jazz'] } };
  await f.open(); assert.equal(f.control.state.draft.mood, ''); assert.equal(f.control.state.draft.direction.include_genres, '');
  assert.equal(f.control.state.draft.expiry, 'until'); assert.equal(f.control.canReview('save'), false);
  assert.equal(f.control.canReview('clear'), false);
});

test('one reviewed steer sends every native constraint through the shared lane', async () => {
  const f = await (await fixture().ready()).open();
  const fields = { include_artists: ['Earth, Wind & Fire'], exclude_artists: ['Other artist'], include_genres: ['jazz'],
    exclude_genres: ['metal'], include_composers: ['Synthetic composer'], exclude_composers: ['Other composer'],
    periods: ['baroque'], exclude_periods: ['modern'], decades: [1960], exclude_decades: [1980],
    include_keywords: ['piano'], exclude_keywords: ['choral'], min_work_minutes: 2, max_work_minutes: 60,
    energy_bias: -2, allow_long_works: true, strict: true };
  for (const [key, value] of Object.entries(fields)) assert.equal(f.control.editDirection(key,
    Array.isArray(value) ? value.join('\n') : typeof value === 'boolean' ? value : String(value)), true);
  f.control.edit('mood', 'bright'); f.control.edit('expiry', 'duration'); f.control.edit('minutes', '90');
  const pending = review(f); assert.ok(Object.isFrozen(pending)); assert.ok(Object.isFrozen(pending.body.constraints.include_artists));
  assert.equal(await f.control.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/dj/steer', body: { zone: 'Studio@S2', mood: 'bright', constraints: fields, for_secs: 5400 } }]);
  assert.equal(f.updates.length, 1); assert.equal(f.control.state.stale, false);
  assert.equal(f.control.state.status.steering.added.energy_bias, -2);
});

test('preserving a deadline uses the final daemon countdown and ignores unrelated track progression', async () => {
  const f = await (await fixture().ready()).open(); review(f);
  getStatus(f).steering.expires_in_secs = 6917; getStatus(f).now = work('Different song');
  getState(f).track = { uri: 'fixture:later', queue_position: 3 };
  assert.equal(await f.control.apply(), true); assert.equal(f.writes()[0].body.for_secs, 6917);
  assert.equal(f.writes()[0].url, '/dj/steer');
});

test('new duration and until-cleared are explicit native expiry choices', async () => {
  for (const [expiry, minutes, seconds] of [['duration', '1', 60], ['duration', '10080', 604800], ['until', '', undefined]]) {
    const f = await (await fixture().ready()).open(); f.control.edit('expiry', expiry); f.control.edit('minutes', minutes);
    review(f); assert.equal(await f.control.apply(), true); assert.equal(f.writes()[0].body.for_secs, seconds);
  }
  for (const minutes of ['', '0', '10081', '1.5', '1e2', ' 1']) {
    const f = await (await fixture().ready()).open(); f.control.edit('expiry', 'duration'); f.control.edit('minutes', minutes);
    assert.equal(f.control.review('save'), null); assert.equal(f.writes().length, 0);
  }
});

test('a deadline too close to preserve is rejected both at review and the final preflight', async () => {
  const f = await fixture().ready(); getStatus(f).steering.expires_in_secs = 59; await f.open();
  assert.equal(f.control.canKeep(), false); assert.equal(f.control.review('save'), null);
  getStatus(f).steering.expires_in_secs = 70; await f.control.read(); review(f);
  getStatus(f).steering.expires_in_secs = 59; assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
});

test('clear is exactly zone and clear, including for a stopped DJ', async () => {
  const f = await fixture().ready(); getStatus(f).running = false; await f.open(); review(f, 'clear');
  assert.equal(await f.control.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/dj/steer', body: { zone: 'Studio@S2', clear: true } }]);
  assert.equal(getStatus(f).running, false);
});

test('saving direction for a stopped DJ does not start playback', async () => {
  const f = await fixture().ready(); getStatus(f).running = false; getState(f).transport_state = 'paused';
  await f.ctl.refresh(); await f.open(); f.control.edit('expiry', 'until'); review(f);
  assert.equal(await f.control.apply(), true); assert.equal(getStatus(f).running, false); assert.equal(getState(f).transport_state, 'paused');
  assert.equal(f.writes()[0].url, '/dj/steer');
});

test('combined start carries direction atomically and addresses the explicitly selected household', async () => {
  const f = await fixture().ready(); f.ctl.select('Studio@S1'); getStatus(f).running = false; getState(f).transport_state = 'paused';
  await f.ctl.refresh(); await f.open(); f.control.editDirection('include_genres', 'jazz');
  f.control.edit('expiry', 'duration'); f.control.edit('minutes', '30'); const pending = review(f, 'start');
  assert.equal(await f.control.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/dj/start', body: { zone: 'Studio@S1', mood: 'calm',
    constraints: { exclude_artists: ['Synthetic avoid'], include_genres: ['jazz'] }, for_secs: 1800 } }]);
});

test('starting rechecks transport, source and duplicate queue occurrence', async () => {
  for (const change of ['transport', 'uri', 'occurrence', 'missing']) {
    const f = await fixture().ready(); getStatus(f).running = false; await f.open(); review(f, 'start');
    if (change === 'transport') getState(f).transport_state = 'paused';
    else if (change === 'uri') getState(f).track.uri = 'fixture:changed';
    else if (change === 'occurrence') getState(f).track.queue_position = 3;
    else delete getState(f).track.queue_position;
    assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('an already-running DJ cannot be started again and running-state changes retire reviewed settings', async () => {
  const f = await (await fixture().ready()).open(); assert.equal(f.control.review('start'), null);
  review(f); getStatus(f).running = false; assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
  await f.ctl.refresh(); assert.equal(await f.control.read(), true); review(f, 'start'); getStatus(f).running = true;
  assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
});

test('changed steering, expiry, selected mood definition or grouping prevents dispatch', async () => {
  for (const change of ['extra', 'effective', 'expiry', 'mood', 'group']) {
    const f = await (await fixture().ready()).open(); review(f);
    if (change === 'extra') getStatus(f).steering.added.include_genres = ['rock'];
    else if (change === 'effective') getStatus(f).steering.constraints.energy_bias = 2;
    else if (change === 'expiry') getStatus(f).steering.expires_at++;
    else if (change === 'mood') f.moods.moods[0].constraints.energy_bias = 2;
    else f.zones[0].members.push('New member');
    assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
  }
});

test('edits and cancel consume an earlier review without mutating its frozen body', async () => {
  const f = await (await fixture().ready()).open(), old = review(f);
  f.control.editDirection('include_artists', 'Later artist'); review(f);
  assert.equal(await f.control.apply(old), false); assert.equal(old.body.constraints.include_artists, undefined);
  f.control.cancelReview(); assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
});

test('draft fields survive observation expiry and explicit reread without silently importing new session extras', async () => {
  const f = await (await fixture().ready()).open(); f.control.editDirection('include_artists', 'Keep my draft');
  f.time(1000 + Direction.MAX_AGE); f.control.observe(); assert.equal(f.control.state.stale, true);
  assert.equal(f.control.state.draft.direction.include_artists, 'Keep my draft'); assert.equal(f.control.review('save'), null);
  getStatus(f).steering.added = { include_artists: ['Another controller'] };
  assert.equal(await f.control.read(), true);
  assert.equal(f.control.state.draft.direction.include_artists, 'Keep my draft');
  assert.deepEqual(f.control.state.status.steering.added.include_artists, ['Another controller']);
  assert.ok(f.control.review('save'));
});

test('hidden, closed, disposed and differently selected lifetimes discard drafts and suppress unsent preflight', async () => {
  for (const action of ['close', 'suspend', 'dispose', 'select', 'controller']) {
    const f = await (await fixture().ready()).open(); review(f);
    const gate = deferred(); let waiting = false;
    f.read(async (url, options, normal) => { if (url === '/zones') { waiting = true; await gate.promise; } return normal(url); });
    const applying = f.control.apply(); await settled(() => waiting);
    if (action === 'select') { f.ctl.state.selected = 'Studio@S1'; f.control.observe(); }
    else if (action === 'controller') { f.ctl.suspend(); f.ctl.resume(); }
    else f.control[action]();
    gate.resolve(); assert.equal(await applying, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('reviews age out inside delayed preflight without waiting for the presentation clock', async () => {
  const f = await (await fixture().ready()).open(); review(f);
  f.read((url, options, normal) => { if (url.endsWith('/dj')) f.time(1000 + Direction.MAX_AGE); return normal(url); });
  assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
});

test('late initial reads cannot resurrect a closed panel or a prior household selection', async () => {
  const f = await fixture().ready(), gate = deferred(); let waiting = false;
  f.read(async (url, options, normal) => { if (url.endsWith('/dj')) { waiting = true; await gate.promise; } return normal(url); });
  const reading = f.control.open(); await settled(() => waiting); f.control.close(); gate.resolve();
  assert.equal(await reading, false); assert.equal(f.control.state.status, null); assert.equal(f.control.state.draft, null);
});

test('malformed or ambiguous moods and unknown constraint fields do not become editable settings', async () => {
  for (const change of ['mood', 'field', 'status', 'line-break']) {
    const f = await fixture().ready();
    if (change === 'mood') f.moods.moods.push({ name: 'Calm', constraints: {} });
    else if (change === 'field') getStatus(f).steering.added.unknown = ['value'];
    else if (change === 'status') getStatus(f).zone = 'Other room';
    else getStatus(f).steering.added.include_artists = ['A\nB'];
    assert.equal(await f.control.open(), false); assert.equal(f.control.state.draft, null); assert.equal(f.writes().length, 0);
  }
});

test('an unlisted saved mood is retained in the draft until the user explicitly chooses a replacement', async () => {
  const f = await fixture().ready(); getStatus(f).steering.mood = 'removed'; getStatus(f).steering.session_mood = 'removed'; await f.open();
  assert.equal(f.control.state.draft.mood, 'removed'); assert.equal(f.control.review('save'), null);
  f.control.edit('mood', 'bright'); assert.ok(f.control.review('save')); assert.equal(await f.control.apply(), true);
});

test('a lost response preserves global uncertainty across passive reads and lifecycle recovery, with no retry', async () => {
  const f = await (await fixture().ready()).open(); const pending = review(f);
  f.write((url, body, normal) => { normal(url, body); throw new Error('Reply lost after storage'); });
  assert.equal(await f.control.apply(pending), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.control.state.result, null);
  f.ctl.suspend(); f.ctl.resume(); await f.ctl.refresh();
  assert.equal(await f.control.apply(pending), false); assert.equal(f.ctl.canManage(), false);
  await f.ctl.refreshStored('schedules', value => value, { explicit: true }); assert.equal(f.ctl.state.uncertain, true);
  assert.equal(f.writes().length, 1); await f.ctl.refresh(true); assert.equal(f.ctl.state.uncertain, false);
});

test('malformed acknowledgements and notes never publish local success', async () => {
  for (const answer of [null, { done: 'Saved' }, { done: 'Saved', changed: 'yes' }, { done: 'Saved', changed: true, notes: [{ code: 'bad' }] }]) {
    const f = await (await fixture().ready()).open(); review(f); f.write(() => answer);
    assert.equal(await f.control.apply(), false); assert.equal(f.control.state.result, null); assert.equal(f.ctl.state.uncertain, true);
  }
});

test('accepted no-op and policy notes survive a readback failure without creating uncertainty', async () => {
  const f = await (await fixture().ready()).open(); review(f);
  f.write((url, body, normal) => {
    normal(url, body); f.read((path, options, read) => { if (path.endsWith('/dj')) throw new Error('Readback unavailable'); return read(path); });
    return { done: 'Already set', changed: false, notes: [{ code: 'NOTE', detail: 'Native explanation' }] };
  });
  assert.equal(await f.control.apply(), true); assert.equal(f.ctl.state.uncertain, false); assert.equal(f.writes().length, 1);
  assert.match(f.control.state.message, /Already set.*Native explanation.*Readback unavailable/);
  assert.equal(f.control.state.result.done, 'Already set Native explanation');
});

test('direction, playback and stored routine writes share one lane even when a sent dialog closes', async () => {
  const f = await (await fixture().ready()).open(); const pending = review(f), gate = deferred(); let sent = false;
  f.write(async (url, body, normal) => { const answer = normal(url, body); sent = true; await gate.promise; return answer; });
  const applying = f.control.apply(); await settled(() => sent); f.control.close();
  assert.equal(await f.ctl.command('pause'), false);
  assert.equal(await f.ctl.mutate(({ send }) => send('/schedules/pause', { id: 7 }), { stored: 'schedules' }), false);
  assert.equal(await f.control.apply(pending), false); gate.resolve(); assert.equal(await applying, true);
  assert.equal(f.writes().length, 1); assert.equal(f.ctl.state.busy, false);
});

test('demo mode can inspect direction but cannot review or dispatch it', async () => {
  const f = await (await fixture({ demo: true }).ready()).open(); f.control.editDirection('include_genres', 'jazz');
  assert.equal(f.control.review('save'), null); assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
});

test('actual dialog handlers edit shared filters with caret preservation and review exact settings', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  get('open-direction').focus(); get('open-direction').fire('click'); await settled(() => f.control.state.status);
  assert.equal(get('direction-dialog').open, true); assert.match(get('direction-effective').textContent, /Only genres: jazz/);
  let input = get('live-direction-include_artists'); input.value = 'Earth, Wind & Fire'; input.selectionStart = 9; input.focus(); input.fire('input');
  input = get('live-direction-include_artists'); assert.equal(f.doc.activeElement, input); assert.equal(input.selectionStart, 9);
  get('direction-expiry').value = 'duration'; get('direction-expiry').fire('change');
  get('direction-minutes').value = '90'; get('direction-minutes').fire('input');
  get('live-direction-strict').checked = true; get('live-direction-strict').fire('change');
  get('direction-review').fire('click'); assert.equal(f.doc.activeElement, get('direction-preview'));
  assert.match(get('direction-preview-settings').textContent, /Earth, Wind & Fire/);
  assert.match(get('direction-preview-settings').textContent, /Strict filters/);
  assert.match(get('direction-preview-expiry').textContent, /90 minutes from saving/); assert.equal(f.writes().length, 0);
  get('direction-confirm').fire('click'); await settled(() => f.control.state.result && !f.control.state.loading);
  assert.equal(f.writes()[0].body.constraints.strict, true);
  assert.equal(f.writes()[0].body.for_secs, 5400); assert.deepEqual(f.writes()[0].body.constraints.include_artists, ['Earth, Wind & Fire']);
  get('direction-dialog').fire('cancel'); assert.equal(get('direction-dialog').open, false);
  assert.equal(f.doc.activeElement, get('open-direction'));
});

test('the live shared editor contains seventeen filters and no saved-routine duration control or note', async () => {
  const f = await (await fixture({ dom: true }).ready()).open(), box = f.doc.getElementById('direction-filters');
  const fields = box.querySelectorAll('[data-direction-focus]');
  assert.equal(fields.length, 17); assert.equal(new Set(fields.map(field => field.id)).size, 17);
  assert.equal(f.doc.getElementById('live-direction-minutes'), null);
  assert.doesNotMatch(box.textContent, /each time this scene or routine/);
  assert.match(box.textContent, /Already queued works stay queued/);
});

test('the full remote editor also preserves explicit automatic mood and requires legacy choice', async () => {
  const doc = documentFixture(), zones = [{ household: 'S2', coordinator_room: 'Studio', members: ['Studio'] }];
  let observed = { zone: 'Studio', running: false, steering: { ...steering(), session_mood: null } };
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
      else if (['value', 'disabled', 'checked'].includes(key)) node[key] = value;
      else node.setAttribute(key, value);
    }
    node.append(...children); return node;
  };
  const remote = S.panel({ el, call: async name => {
    if (name === 'djSteeringZones') return copy(zones);
    if (name === 'djSteeringMoods') return { moods: [{ name: 'calm' }] };
    if (name === 'djSteeringStatus') return copy(observed);
    throw new Error('Unexpected full remote write');
  } });
  const descendants = node => [node, ...node.children.flatMap(child => typeof child === 'object' ? descendants(child) : [])];
  await remote.model.load(); await remote.model.select('Studio@S2');
  const nodes = descendants(remote.node), mood = nodes.find(node => node.id?.endsWith('-mood'));
  const button = nodes.find(node => node.tagName === 'BUTTON' && node.textContent === 'Review steering');
  assert.equal(mood.value, 'null'); button.fire('click');
  assert.equal(remote.model.state.pending.body.mood, undefined);
  delete observed.steering.session_mood; await remote.model.select('Studio@S2');
  assert.equal(mood.value, ''); button.fire('click'); assert.equal(remote.model.state.pending, null);
  mood.value = JSON.stringify('calm'); mood.fire('change'); button.fire('click');
  assert.equal(remote.model.state.pending.body.mood, 'calm');
  remote.dispose();
});

test('browser helper exports can load before the dashboard panel without a full-remote writer', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src/remote'), context = vm.createContext({});
  for (const name of ['dashboard-core.js', 'dj-direction.js', 'dj-steering.js', 'dashboard-direction.js']) {
    vm.runInContext(fs.readFileSync(path.join(root, name), 'utf8'), context);
  }
  assert.equal(vm.runInContext('typeof DashboardDirection.panel', context), 'function');
});

test('real loopback HTTP sends one native combined start with the reviewed direction', async t => {
  let backend; const received = [];
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : undefined; received.push({ url: req.url, method: req.method, body });
    const value = await backend(req.url, body); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await fixture({ network(raw) { backend = raw; return C.request({
    fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) }); } }).ready();
  getStatus(f).running = false; await f.open(); f.control.editDirection('include_artists', 'Earth, Wind & Fire');
  f.control.edit('expiry', 'duration'); f.control.edit('minutes', '60'); review(f, 'start');
  assert.equal(await f.control.apply(), true);
  assert.deepEqual(received.filter(item => item.method === 'POST'), [{ url: '/dj/start', method: 'POST',
    body: { zone: 'Studio@S2', mood: 'calm', constraints: { include_artists: ['Earth, Wind & Fire'], exclude_artists: ['Synthetic avoid'] }, for_secs: 3600 } }]);
});

for (const action of ['save', 'start']) test(`real HTTP rehearsal handoff requires explicit ${action} and sends only the reviewed direction`, async t => {
  let backend; const received = [];
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : undefined; received.push({ url: req.url, method: req.method, body });
    const value = await backend(req.url, body); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await fixture({ network(raw) { backend = raw; return C.request({
    fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) }); } }).ready();
  f.ctl.select('Studio@S1'); getStatus(f).running = action !== 'start';
  f.write((url, body, native) => {
    if (url !== '/dj/rehearse') return native(url, body);
    const from = '2026-11-08T23:40:00-04:00';
    return { zone: 'Studio', from, seed: body.seed, works: [{ at: from, work: work('Rehearsed whole work'), energy: 40, explicit: false }],
      summary: { works: 1, minutes: 5, distinct_artists: 1, explicit: 0,
        hours: [{ from, works: 1, energy: 40, artist_entropy: 0, genre_entropy: 0 }] } };
  });
  const rehearsal = Rehearsal.model({ call: f.call, controller: f.ctl });
  rehearsal.open(); await rehearsal.loadMoods();
  assert.equal(await rehearsal.generate({ lengthMode: 'hours', length: '2', at: '23:40', seed: '7', mood: 'bright',
    filters: { include_artists: 'Earth, Wind & Fire', strict: true } }), true);
  const captured = rehearsal.handoff(); rehearsal.close();
  assert.equal(await f.control.importRehearsal(captured), true);
  assert.equal(f.control.review(action), null, 'planning duration does not become a live expiry');
  assert.equal(received.filter(row => row.method === 'POST').length, 1, 'only the read-only rehearsal was posted');
  f.control.edit('expiry', 'duration'); f.control.edit('minutes', '90'); review(f, action);
  assert.equal(received.filter(row => row.method === 'POST').length, 1, 'review still sends nothing');
  assert.equal(await f.control.apply(), true);
  assert.deepEqual(received.filter(row => row.method === 'POST'), [
    { url: '/dj/rehearse', method: 'POST', body: { zone: 'Studio@S1', hours: 2, seed: 7, at: '23:40', mood: 'bright',
      constraints: { include_artists: ['Earth, Wind & Fire'], strict: true } } },
    { url: action === 'start' ? '/dj/start' : '/dj/steer', method: 'POST', body: { zone: 'Studio@S1', mood: 'bright',
      constraints: { include_artists: ['Earth, Wind & Fire'], strict: true }, for_secs: 5400 } },
  ]);
  assert.equal(f.control.state.imported, null); assert.equal(f.ctl.state.uncertain, false);
});


test('editing another field preserves strict session filters with a pinned or inherited mood', async () => {
  for (const sessionMood of ['calm', null]) {
    const f = await fixture().ready(), current = getStatus(f).steering;
    current.session_mood = sessionMood;
    current.constraints.strict = true; current.added.strict = true;
    await f.open();
    assert.equal(f.control.state.draft.direction.strict, true);
    f.control.editDirection('include_artists', 'Another synthetic artist');
    const pending = review(f);
    assert.equal(pending.body.constraints.strict, true);
    assert.equal(pending.body.mood, sessionMood ?? undefined);
    assert.equal(await f.control.apply(pending), true);
    assert.equal(f.writes().length, 1);
    assert.equal(f.writes()[0].body.constraints.strict, true);
    assert.equal(f.control.state.draft.direction.strict, true);
  }
});

test('strict filter summaries describe exhaustion rather than interpreting booleans as durations', () => {
  assert.equal(Direction.describeFilters({ strict: true }), 'Strict filters; new picks wait if none match');
  assert.equal(Direction.describeFilters({ strict: false }), 'No additional filters');
  const described = Direction.describeFilters({ strict: true, max_work_minutes: 20 });
  assert.match(described, /Strict filters/);
  assert.match(described, /Maximum work length 20 minutes/);
  assert.doesNotMatch(described, /true minutes/);
});

test('unavailable steering remains visible and can be explicitly cleared to recover selection', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  getStatus(f).steering = { source: 'unavailable', session_mood: null };
  getStatus(f).problem = 'DJ steering could not be read. New selections are deferred; queued playback continues.';
  await f.open();
  assert.match(get('direction-effective').textContent, /new selections are deferred/);
  assert.match(get('direction-added').textContent, /unavailable/);
  assert.match(get('direction-deadline').textContent, /unavailable/);
  assert.doesNotMatch(get('direction-deadline').textContent, /Following the time-of-day program/);
  assert.match(get('direction-problem').textContent, /queued playback continues/);
  assert.equal(f.control.state.status.next.length, 1);
  assert.equal(get('direction-clear').disabled, false);
  get('direction-clear').fire('click');
  assert.equal(f.writes().length, 0);
  get('direction-confirm').fire('click');
  await settled(() => f.control.state.result && !f.control.state.loading);
  assert.deepEqual(f.writes(), [{ url: '/dj/steer', body: { zone: 'Studio@S2', clear: true } }]);
  assert.match(get('direction-deadline').textContent, /Following the time-of-day program/);
});
