'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Rehearsal = require('../crates/fsonos-api/src/remote/dashboard-rehearsal.js');
const clone = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
async function settled(predicate) {
  for (let n = 0; n < 100 && !predicate(); n++) await new Promise(resolve => setImmediate(resolve));
  assert.ok(predicate(), 'the operation reached its expected boundary');
}
const zones = () => [
  { household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen'], transport_state: 'playing' },
  { household: 'S1', coordinator_room: 'Studio', members: ['Studio'], transport_state: 'paused' },
];
const report = (seed = 1) => ({ zone: 'Studio', from: '2026-11-08T23:40:00-04:00', seed,
  works: [
    { at: '2026-11-08T23:40:00-04:00', work: { title: '<Whole work & song>', composer: 'Fixture artist',
      movements: 3, minutes: 24, performers: ['Fixture quartet'], album: 'Fixture album',
      reason: { summary: 'Variety and the owner’s preferences.', relaxed: ['keywords'], factors: [{ factor: 'feedback', weight: 1250 }] } },
    energy: 35, energy_target: 40, genre: 'jazz', mood: 'focus', explicit: false },
    { at: '2026-11-09T00:04:00-04:00', work: { title: 'After midnight', composer: 'Another artist',
      movements: 1, minutes: 5, reason: { summary: 'Artist spacing.' } }, energy: 20, explicit: true },
  ], summary: { works: 2, minutes: 29, distinct_artists: 2, explicit: 1, hours: [
    { from: '2026-11-08T23:40:00-04:00', works: 1, energy: 35, energy_target: 40, artist_entropy: 0, genre_entropy: 0 },
    { from: '2026-11-09T00:04:00-04:00', works: 1, energy: 20, artist_entropy: 0, genre_entropy: 0 },
  ] } });

function documentFixture() {
  const ids = new Map(), doc = { activeElement: null };
  const descendants = node => node.children.flatMap(child => [child, ...descendants(child)]);
  doc.createElement = tag => {
    const listeners = new Map(); let id = '', text = '';
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', checked: false, disabled: false, open: false, hidden: false,
      get id() { return id; }, set id(value) { id = value; ids.set(value, this); },
      get textContent() { return text + this.children.map(node => node.textContent).join(' '); }, set textContent(value) { text = String(value); this.children = []; },
      append(...nodes) { this.children.push(...nodes); }, replaceChildren(...nodes) { text = ''; this.children = nodes; },
      setAttribute(key, value) { this.attributes[key] = String(value); }, getAttribute(key) { return this.attributes[key] ?? null; },
      addEventListener(event, handler) { const list = listeners.get(event) || []; list.push(handler); listeners.set(event, list); },
      fire(event) { if (!this.disabled) for (const handler of listeners.get(event) || []) handler({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; },
      find(tag) { return descendants(this).filter(node => node.tagName === tag.toUpperCase()); } };
  };
  doc.getElementById = id => ids.get(id) || null;
  const trigger = doc.createElement('button'); trigger.id = 'open-rehearsal';
  const dialog = doc.createElement('dialog'); dialog.id = 'rehearsal-dialog';
  return doc;
}

function fixture({ dom = false, remote, demo = false } = {}) {
  const inventory = zones(), calls = [];
  let planner = null, reader = null, preview;
  const raw = async (url, body, options) => {
    calls.push({ url, body });
    if (body !== undefined) {
      assert.equal(url, '/dj/rehearse', 'a rehearsal never sends a playback or steering command');
      return planner ? planner(body) : report(body.seed);
    }
    if (url === '/zones') return reader ? reader() : clone(inventory);
    if (url === '/dj/moods') return { moods: [{ name: 'focus' }, { name: 'bright' }] };
    const match = /^\/zones\/([^/]+)\/state$/.exec(url);
    if (match) { const zone = inventory.find(item => C.key(item) === decodeURIComponent(match[1])); return { zone: clone(zone), transport_state: zone.transport_state }; }
    throw new Error('Unexpected observation ' + url);
  };
  const call = remote ? remote(raw) : raw;
  const controller = C.model({ call, demo, changed: () => preview?.observe() });
  const doc = dom ? documentFixture() : null;
  const panel = dom ? Rehearsal.panel({ call, controller, document: doc, now: () => 1234, demo }) : null;
  preview = panel ? panel.control : Rehearsal.model({ call, controller, now: () => 1234, demo });
  return { calls, inventory, controller, preview, panel, doc, raw,
    planner(value) { planner = value; }, reader(value) { reader = value; },
    posts() { return calls.filter(row => row.body !== undefined); },
    async ready() { assert.equal(await controller.refresh(), true); controller.connection(true); preview.observe(); return this; } };
}

test('rehearsal is lazy and requires a current selected group without needing playback controls', async () => {
  const f = fixture(); assert.equal(f.preview.open(), false); assert.deepEqual(f.calls, []);
  await f.ready(); const count = f.calls.length;
  assert.equal(f.controller.canControl(), true); assert.equal(f.preview.open(), true);
  for (let i = 0; i < 5; i++) f.preview.observe();
  assert.equal(f.calls.length, count, 'opening and observing do not plan or poll');
  f.controller.state.busy = true; assert.equal(await f.preview.generate({}), false);
  f.controller.state.busy = false; assert.equal(await f.preview.generate({}), true);
  assert.equal(f.controller.state.busy, false); assert.equal(f.controller.state.uncertain, false);
});

test('design demo allows browsing settings but never requests or fabricates a rehearsal', async () => {
  const f = await fixture({ dom: true, demo: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-rehearsal').fire('click'); await settled(() => !f.preview.state.loadingMoods);
  assert.equal($('rehearsal-dialog').open, true); assert.equal(f.preview.available(), true);
  assert.equal(f.controller.canControl(), false); assert.equal(f.preview.canGenerate(), false);
  assert.equal($('rehearsal-generate').disabled, true);
  assert.equal($('rehearsal-message').textContent, 'Connect to your running house to rehearse from its library.');
  $('rehearsal-mode').value = 'hours'; $('rehearsal-mode').fire('change');
  assert.equal($('rehearsal-length').value, '2');
  $('rehearsal-form').fire('submit'); assert.equal(await f.preview.generate({}), false);
  assert.equal(f.posts().length, 0); assert.equal(f.preview.state.report, null); assert.equal($('rehearsal-output').hidden, true);
});

for (const house of ['S1', 'S2']) test(`plans the selected ${house} Studio with one POST and the existing request contract`, async () => {
  const f = await fixture().ready(); f.controller.select('Studio@' + house); f.preview.open();
  await f.preview.loadMoods();
  assert.equal(await f.preview.generate({ lengthMode: 'hours', length: '2', at: '23:40', seed: '0', mood: 'focus',
    filters: { include_artists: 'Earth, Wind & Fire\nAnother artist', strict: true } }), true);
  assert.deepEqual(f.posts(), [{ url: '/dj/rehearse', body: { zone: 'Studio@' + house, hours: 2, at: '23:40', seed: 0, mood: 'focus',
    constraints: { include_artists: ['Earth, Wind & Fire', 'Another artist'], strict: true } } }]);
  assert.equal(f.preview.state.generatedAt, 1234); assert.equal(f.preview.state.report.seed, 0);
  assert.equal(f.preview.state.zone.target, 'Studio@' + house);
  assert.equal(f.controller.state.uncertain, false);
});

test('the adapter delegates validation and does not dispatch invalid dates, limits or filters', async () => {
  const f = await fixture().ready(); f.preview.open(); const count = f.calls.length;
  for (const draft of [{ lengthMode: 'hours', length: '49' }, { at: '2026-02-29T23:00:00Z' },
    { seed: '1e3' }, { mood: 'unknown' }, { filters: { min_work_minutes: '50', max_work_minutes: '20' } }]) {
    assert.equal(await f.preview.generate(draft), false);
  }
  assert.equal(f.calls.length, count);
});

test('group membership is verified on both sides of planning', async () => {
  for (const changedRead of [1, 2]) {
    const f = await fixture().ready(); f.preview.open(); let reads = 0;
    f.reader(() => ++reads === changedRead ? [{ ...f.inventory[0], members: ['Studio'] }, f.inventory[1]] : clone(f.inventory));
    assert.equal(await f.preview.generate({}), false); assert.equal(f.preview.state.report, null);
    assert.match(f.preview.state.message, /group changed/); assert.equal(f.posts().length, changedRead === 1 ? 0 : 1);
  }
});

test('switching household discards a late plan and keeps its request slot occupied until completion', async () => {
  const f = await fixture().ready(), gate = deferred(); f.preview.open(); f.planner(() => gate.promise);
  const pending = f.preview.generate({}); await settled(() => f.posts().length === 1);
  f.controller.select('Studio@S1');
  assert.equal(f.preview.state.zone.target, 'Studio@S1'); assert.equal(f.preview.state.busy, true);
  assert.equal(await f.preview.generate({}), false); gate.resolve(report());
  assert.equal(await pending, false); assert.equal(f.preview.state.report, null); assert.equal(f.posts().length, 1);
});

for (const action of ['close', 'suspend', 'dispose']) test(`${action} fences late results without automatic retry`, async () => {
  const f = await fixture().ready(), gate = deferred(); f.preview.open(); f.planner(() => gate.promise);
  const pending = f.preview.generate({}); await settled(() => f.posts().length === 1);
  f.preview[action]();
  assert.equal(await f.preview.generate({}), false);
  if (action === 'suspend') { f.preview.resume(); assert.equal(f.preview.state.open, false); }
  gate.resolve(report()); assert.equal(await pending, false); assert.equal(f.preview.state.report, null);
  assert.equal(f.posts().length, 1); assert.equal(f.controller.state.uncertain, false);
});

test('closing during the initial group read prevents the read-only POST', async () => {
  const f = await fixture().ready(), gate = deferred(); f.preview.open(); f.reader(() => gate.promise);
  const pending = f.preview.generate({}); f.preview.close(); gate.resolve(zones());
  assert.equal(await pending, false); assert.equal(f.posts().length, 0);
});

test('stale playback keeps its labeled snapshot but cannot authorize another generation', async () => {
  const f = await fixture().ready(); f.preview.open();
  f.controller.state.selected = 'Studio@S1';
  assert.equal(await f.preview.generate({}), false, 'the adapter independently checks its captured target');
  f.preview.observe(); assert.equal(await f.preview.generate({}), true);
  const captured = f.preview.state.report;
  f.controller.invalidate(['Studio@S1']); assert.equal(f.preview.state.report, captured); assert.equal(f.preview.available(), false);
  assert.equal(await f.preview.generate({}), false); assert.equal(f.posts().length, 1);
  await f.controller.refresh(); assert.equal(f.preview.state.report, captured); assert.equal(f.preview.available(), true);
  f.controller.invalidate(); assert.equal(f.preview.state.report, captured); assert.equal(f.preview.available(), false);
  f.inventory[1].members.push('Hall'); await f.controller.refresh();
  assert.equal(f.preview.state.report, null, 'an actual membership change retires the snapshot');
});

test('ordinary live playback invalidation cannot discard a pending plan for an unchanged group', async () => {
  const f = await fixture().ready(), gate = deferred(); f.preview.open(); f.planner(() => gate.promise);
  const pending = f.preview.generate({}); await settled(() => f.posts().length === 1);
  f.controller.invalidate(['Studio@S2']); gate.resolve(report());
  assert.equal(await pending, true, 'the shared planner verifies actual membership after the reply');
  assert.equal(f.preview.state.report.works.length, 2); assert.equal(f.preview.available(), false);
  await f.controller.refresh(); assert.equal(f.preview.available(), true); assert.equal(f.posts().length, 1);
});

test('failed or invalid results never become playback uncertainty and do not retry themselves', async () => {
  const f = await fixture().ready(); f.preview.open();
  f.planner(() => { throw Object.assign(new Error('No response'), { uncertain: true }); });
  assert.equal(await f.preview.generate({}), false); assert.match(f.preview.state.message, /only plans a set/);
  assert.equal(f.controller.state.uncertain, false); assert.equal(f.posts().length, 1);
  f.planner(() => ({ ...report(), zone: 'Another room' }));
  assert.equal(await f.preview.generate({}), false); assert.equal(f.preview.state.report, null);
  assert.equal(f.posts().length, 2);
});

for (const failure of ['rejected fetch', 'timeout']) test(`C.request ${failure} explains read-only planning without command uncertainty`, async () => {
  let attempts = 0;
  const f = await fixture({ remote: raw => C.request({
    schedule(fn, milliseconds) {
      if (failure === 'timeout' && milliseconds === 600000) { queueMicrotask(fn); return null; }
      return setTimeout(fn, milliseconds);
    },
    cancel(timer) { if (timer !== null) clearTimeout(timer); },
    fetcher: async (url, init) => {
      if (url === '/dj/rehearse') {
        ++attempts;
        if (failure === 'timeout') return new Promise(() => {});
        throw new TypeError('Network request failed');
      }
      const value = await raw(url);
      return { ok: true, text: async () => JSON.stringify(value) };
    },
  }) }).ready();
  f.preview.open(); assert.equal(await f.preview.generate({}), false);
  assert.equal(attempts, 1); assert.equal(f.preview.state.report, null);
  assert.match(f.preview.state.message, /only plans a set; it does not play or queue music/);
  assert.doesNotMatch(f.preview.state.message, /command confirmation|may still complete/);
  assert.equal(f.controller.state.uncertain, false); assert.equal(f.controller.state.busy, false);
});

test('C.request HTTP errors keep their server detail and hint instead of a transport-failure explanation', async () => {
  let attempts = 0;
  const f = await fixture({ remote: raw => C.request({ fetcher: async url => {
    if (url === '/dj/rehearse') {
      ++attempts;
      return { ok: false, status: 429, headers: { get: () => '2' },
        text: async () => JSON.stringify({ detail: 'The planner is busy.', hint: 'Wait two seconds before generating again.' }) };
    }
    const value = await raw(url);
    return { ok: true, text: async () => JSON.stringify(value) };
  } }) }).ready();
  f.preview.open(); assert.equal(await f.preview.generate({}), false);
  assert.equal(attempts, 1);
  assert.equal(f.preview.state.message, 'The planner is busy. Wait two seconds before generating again.');
  assert.equal(f.controller.state.uncertain, false); assert.equal(f.preview.state.report, null);
});

test('editing after planning keeps the captured request and marks its snapshot without live steering handoff', async () => {
  const f = await fixture().ready(); f.preview.open(); await f.preview.loadMoods();
  assert.equal(await f.preview.generate({ mood: 'focus' }), true);
  const planned = f.preview.state.report, captured = f.preview.state.request;
  f.preview.edit(); assert.equal(f.preview.state.edited, true); assert.equal(f.preview.state.report, planned);
  assert.equal(f.preview.state.request, captured); assert.equal(captured.mood, 'focus');
  assert.equal(f.preview.handoff, undefined); assert.equal(f.posts().length, 1);
});

test('the actual HTTP adapter sends the read-only POST once between independent group reads', async t => {
  const received = [];
  const server = http.createServer(async (req, res) => {
    let text = ''; for await (const chunk of req) text += chunk;
    const body = text ? JSON.parse(text) : undefined;
    received.push({ method: req.method, url: req.url, body });
    let data;
    if (req.url === '/zones') data = zones();
    else if (/\/state$/.test(req.url)) data = { zone: zones().find(zone => C.key(zone) === decodeURIComponent(req.url.split('/')[2])), transport_state: 'playing' };
    else if (req.url === '/dj/moods') data = { moods: [{ name: 'focus' }] };
    else if (req.method === 'POST' && req.url === '/dj/rehearse') data = report(body.seed);
    else { res.writeHead(404); res.end('{}'); return; }
    res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(data));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const call = C.request({ fetcher: (url, init) => fetch(origin + url, init) });
  let preview; const controller = C.model({ call, changed: () => preview?.observe() });
  preview = Rehearsal.model({ call, controller });
  assert.equal(await controller.refresh(), true); preview.open(); await preview.loadMoods();
  const before = received.length; assert.equal(await preview.generate({ length: '2', at: '23:40', seed: '7', mood: 'focus' }), true);
  assert.deepEqual(received.slice(before), [
    { method: 'GET', url: '/zones', body: undefined },
    { method: 'POST', url: '/dj/rehearse', body: { zone: 'Studio@S2', works: 2, seed: 7, at: '23:40', mood: 'focus' } },
    { method: 'GET', url: '/zones', body: undefined },
  ]);
  assert.equal(preview.state.report.works.length, 2); assert.equal(controller.state.uncertain, false);
});

test('the panel presents actual whole works, reasons, midnight offsets, hourly energy and variety as literal text', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-rehearsal').fire('click'); await settled(() => !f.preview.state.loadingMoods);
  assert.equal($('rehearsal-dialog').open, true); $('rehearsal-form').fire('submit');
  await settled(() => !f.preview.state.busy && f.preview.state.report !== null);
  const output = $('rehearsal-output'), text = output.textContent;
  for (const phrase of ['<Whole work & song>', '3 movements, kept together', 'Variety and the owner’s preferences.',
    'Filters relaxed for this pick: keywords', 'feedback: ×1.25', '2026-11-09 00:04:00 -04:00', '35 / 100', 'Target 40', '0.00 bits']) assert.ok(text.includes(phrase), phrase);
  assert.equal(output.find('table').length, 1); assert.equal(output.find('meter').length, 2);
  assert.equal(output.find('script').length, 0); assert.equal(output.find('img').length, 0);
  assert.equal(output.find('meter')[0].min, 0); assert.equal(output.find('meter')[0].max, 100);
  assert.equal(output.find('meter')[0].value, 35); assert.equal(f.posts().length, 1);
  $('rehearsal-form').fire('input'); assert.match($('rehearsal-snapshot').textContent, /Settings changed/);
  $('close-rehearsal').fire('click'); assert.equal($('rehearsal-dialog').open, false);
  assert.equal(f.doc.activeElement, $('open-rehearsal'));
});

test('the panel submits form controls through native validation and offers bounded timeline expansion', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-rehearsal').fire('click'); await settled(() => !f.preview.state.loadingMoods);
  const many = report(); many.works = Array.from({ length: 30 }, (_, index) => ({ ...report().works[0],
    at: new Date(Date.parse(report().from) + index * 60_000).toISOString(), explicit: false }));
  many.from = many.works[0].at; many.summary = { works: 30, minutes: 30, distinct_artists: 1, explicit: 0,
    hours: [{ ...report().summary.hours[0], from: many.from, works: 30 }] };
  f.planner(body => ({ ...many, seed: body.seed }));
  $('rehearsal-mode').value = 'hours'; $('rehearsal-mode').fire('change');
  assert.equal($('rehearsal-length').value, '2'); assert.equal($('rehearsal-length').max, '48');
  $('rehearsal-seed').value = '0'; $('rehearsal-mood').value = 'focus'; $('rehearsal-include_artists').value = 'Earth, Wind & Fire';
  $('rehearsal-strict').checked = true; $('rehearsal-form').fire('submit');
  await settled(() => f.preview.state.report !== null);
  assert.deepEqual(f.posts()[0].body, { zone: 'Studio@S2', hours: 2, seed: 0, mood: 'focus', constraints: { include_artists: ['Earth, Wind & Fire'], strict: true } });
  assert.equal($('rehearsal-output').find('article').length, 24); assert.equal($('rehearsal-more').hidden, false);
  $('rehearsal-more').fire('click'); assert.equal($('rehearsal-output').find('article').length, 30); assert.equal($('rehearsal-more').hidden, true);
  $('rehearsal-clear').fire('click'); assert.equal($('rehearsal-include_artists').value, ''); assert.equal($('rehearsal-strict').checked, false);
  assert.equal(f.preview.state.edited, true); assert.equal(f.posts().length, 1);
});
