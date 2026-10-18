'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const steering = require('../crates/fsonos-api/src/remote/dj-steering.js');
const zones = () => [
  { household: 'S1', coordinator_room: 'Study', members: ['Study', 'Hall'] },
  { household: 'S2', coordinator_room: 'Study', members: ['Study'] },
];
const zone = (household = 'S2') => steering.zonesOf(zones()).find((z) => z.target === `Study@${household}`);
const status = () => ({ zone: 'Study', running: true, steering: { source: 'session', mood: 'focus',
  constraints: { include_genres: ['jazz'] }, added: { include_genres: ['jazz'] },
  expires_at: 1800000600, expires_in_secs: 600 } });
const settings = () => ({ mood: 'calm', constraints: { include_artists: ['Earth, Wind & Fire', '坂本龍一'],
  exclude_keywords: ['<script>not markup</script>'], min_work_minutes: 2, max_work_minutes: 40,
  energy_bias: -1, allow_long_works: true } });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
const tick = () => new Promise((resolve) => setImmediate(resolve));
function fixture(overrides = {}) {
  const calls = [];
  const m = steering.model({ call: async (name, args) => {
    calls.push([name, args]);
    if (overrides.call) return overrides.call(name, args);
    if (name === 'djSteeringZones') return zones();
    if (name === 'djSteeringMoods') return { moods: [{ name: 'focus' }, { name: 'calm' }] };
    if (name === 'djSteeringStatus') return status();
    if (name === 'djSteeringWrite') return { done: 'Steering saved', changed: true };
    throw new Error(`Unexpected call: ${name}`);
  } });
  return { m, calls };
}

for (const household of ['S1', 'S2']) test(`imports an explicit ${household} draft without changing observed state or sending a write`, async () => {
  const { m, calls } = fixture();
  const input = settings();
  assert.equal(await m.openFor(zone(household), input), true);
  input.constraints.include_artists.push('External change');
  input.mood = 'focus';
  assert.deepEqual(m.state.imported, settings());
  assert.equal(Object.isFrozen(m.state.imported.constraints.include_artists), true);
  assert.equal(m.state.status.steering.mood, 'focus');
  assert.equal(m.state.pending, null);
  assert.equal(m.state.target, `Study@${household}`);
  assert.match(m.state.message, /unsaved draft/);
  assert.deepEqual(calls.map(([name]) => name), ['djSteeringZones', 'djSteeringMoods', 'djSteeringStatus']);
  assert.equal(calls[2][1].params.room, `Study@${household}`);
});

test('the import boundary permits explicit settings only, preserving names and canonical filters', () => {
  assert.deepEqual(steering.settingsOf({ constraints: { decades: [1960], periods: ['Late Romantic'],
    include_artists: ['  Earth, Wind & Fire  '], energy_bias: 0, allow_long_works: false } }),
  { constraints: { include_artists: ['Earth, Wind & Fire'], periods: ['late_romantic'], decades: [1960] } });
  for (const invalid of [null, [], {}, { constraints: {} }, { mood: '' }, { mood: null },
    { mood: 'calm', seed: 1 }, { mood: 'calm', at: '23:00' }, { mood: 'calm', hours: 3 },
    { mood: 'calm', zone: 'Study@S1' }, { mood: 'calm', clear: true },
    { mood: 'calm', for_secs: 60 }, { mood: 'calm', constraints: { unknown: [] } },
    { constraints: { include_artists: ['One\nTwo'] } }, { constraints: { include_genres: [7] } },
    { constraints: { include_genres: Array(33).fill('jazz') } },
    { constraints: { min_work_minutes: 8, max_work_minutes: 2 } }]) {
    assert.throws(() => steering.settingsOf(invalid), JSON.stringify(invalid));
  }
});

test('invalid imports do not perform even a read or discard an existing selection', async () => {
  const { m, calls } = fixture();
  await m.openFor(zone()); const before = m.state.status;
  const count = calls.length;
  assert.equal(await m.openFor(zone(), { mood: 'calm', seed: 1 }), false);
  assert.equal(calls.length, count);
  assert.equal(m.state.status, before);
  assert.equal(m.state.target, 'Study@S2');
});

test('a removed mood prevents importing rather than silently falling back to the program', async () => {
  const { m, calls } = fixture({ call: async (name) => name === 'djSteeringZones' ? zones()
    : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }] } : status() });
  assert.equal(await m.openFor(zone(), settings()), false);
  assert.equal(m.state.ready, false);
  assert.equal(m.state.imported, null);
  assert.match(m.state.message, /Choose a mood/);
  assert.equal(calls.some(([name]) => name === 'djSteeringStatus' || name === 'djSteeringWrite'), false);
});

test('changed group membership rejects a draft instead of choosing another coordinator or household', async () => {
  const { m, calls } = fixture({ call: async (name) => name === 'djSteeringZones'
    ? [{ ...zones()[0], members: ['Study'] }] : { moods: [{ name: 'calm' }] } });
  assert.equal(await m.openFor(zone('S1'), settings()), false);
  assert.equal(m.state.target, '');
  assert.equal(m.state.imported, null);
  assert.equal(calls.some(([name]) => name === 'djSteeringStatus'), false);
});

test('a newer handoff to the same group wins over a delayed old status response', async () => {
  const delayed = deferred(); let reads = 0;
  const { m } = fixture({ call: async (name) => name === 'djSteeringZones' ? zones()
    : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }, { name: 'calm' }] }
      : ++reads === 1 ? delayed.promise : status() });
  const old = m.openFor(zone(), settings()); await tick();
  assert.equal(await m.openFor(zone(), { mood: 'focus' }), true);
  delayed.resolve(status()); assert.equal(await old, false);
  assert.deepEqual(m.state.imported, { mood: 'focus', constraints: {} });
});

for (const action of ['close', 'dispose']) test(`${action} fences a late imported draft`, async () => {
  const delayed = deferred();
  const { m, calls } = fixture({ call: async (name) => name === 'djSteeringZones' ? zones()
    : name === 'djSteeringMoods' ? { moods: [{ name: 'calm' }] } : delayed.promise });
  const opening = m.openFor(zone(), settings()); await tick(); m[action](); delayed.resolve(status());
  assert.equal(await opening, false);
  assert.equal(m.state.imported, null);
  assert.equal(m.state.open, false);
  assert.equal(calls.some(([name]) => name === 'djSteeringWrite'), false);
});

for (const drafted of ['edit', 'review']) test(`a rehearsal cannot silently replace an unfinished ${drafted}`, async () => {
  const { m, calls } = fixture();
  await m.openFor(zone());
  if (drafted === 'edit') m.edit(); else m.review({ mood: 'focus', minutes: '15' });
  const pending = m.state.pending, count = calls.length;
  assert.equal(await m.openFor(zone('S1'), settings()), false);
  assert.equal(calls.length, count);
  assert.equal(m.state.pending, pending);
  assert.equal(m.state.target, 'Study@S2');
  assert.match(m.state.message, /unfinished draft/);
  m.close(); assert.equal(await m.openFor(zone('S1'), settings()), true);
});

test('manual selection and refresh clear the imported draft instead of transferring filters to a different group', async () => {
  const { m } = fixture();
  await m.openFor(zone(), settings());
  await m.select('Study@S1'); assert.equal(m.state.imported, null);
  await m.openFor(zone(), settings());
  await m.load(); assert.equal(m.state.imported, null);
  assert.equal(m.state.status.steering.mood, 'focus');
});

test('an imported draft still requires review and exactly one fresh, group-scoped save', async () => {
  const { m, calls } = fixture();
  await m.openFor(zone(), settings());
  assert.equal(await m.confirm({ body: m.state.imported }), false);
  const review = m.review({ ...m.state.imported, minutes: '45' });
  assert.ok(review);
  assert.equal(await m.confirm(review), true);
  assert.equal(await m.confirm(review), false);
  const writes = calls.filter(([name]) => name === 'djSteeringWrite');
  assert.deepEqual(writes, [['djSteeringWrite', { body: { zone: 'Study@S2', ...settings(), for_secs: 2700 } }]]);
  assert.equal(m.state.imported, null);
  assert.equal(m.state.edited, false);
});

test('steering changed after import is a definite preflight failure, not permission to overwrite it', async () => {
  let changed = false;
  const { m, calls } = fixture({ call: async (name) => name === 'djSteeringZones' ? zones()
    : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }, { name: 'calm' }] }
      : { ...status(), steering: { ...status().steering, mood: changed ? 'calm' : 'focus' } } });
  await m.openFor(zone(), settings()); const review = m.review({ ...m.state.imported, minutes: '45' });
  changed = true; assert.equal(await m.confirm(review), false);
  assert.equal(calls.some(([name]) => name === 'djSteeringWrite'), false);
  assert.match(m.state.message, /steering changed/);
});

test('a sent save retains its lane and a lost answer is never replayed or reported as confirmed', async () => {
  const held = deferred();
  const { m, calls } = fixture({ call: async (name) => name === 'djSteeringZones' ? zones()
    : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }, { name: 'calm' }] }
      : name === 'djSteeringWrite' ? held.promise : status() });
  await m.openFor(zone(), settings()); const review = m.review({ ...m.state.imported, minutes: '45' });
  const saving = m.confirm(review); await tick();
  assert.equal(await m.openFor(zone('S1'), { mood: 'focus' }), false);
  assert.equal(await m.confirm(review), false);
  held.reject(new Error('Connection lost')); assert.equal(await saving, false);
  assert.match(m.state.message, /Outcome uncertain/);
  assert.equal(m.state.ready, false);
  assert.equal(calls.filter(([name]) => name === 'djSteeringWrite').length, 1);
});

test('real localhost JSON uses the existing steering endpoint and sends only reviewed settings', async (t) => {
  const seen = [];
  const server = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    seen.push([req.method, req.url, body && JSON.parse(body)]);
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.url === '/zones' ? zones() : req.url === '/dj/moods'
      ? { moods: [{ name: 'focus' }, { name: 'calm' }] } : req.method === 'POST'
        ? { done: 'Steering saved', changed: true } : status()));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`;
  const m = steering.model({ call: async (name, { params = {}, body } = {}) => {
    const [method, template] = steering.routes[name];
    const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    const response = await fetch(base + path, { method, headers: { 'Content-Type': 'application/json' },
      ...(body ? { body: JSON.stringify(body) } : {}) });
    return response.json();
  } });
  await m.openFor(zone('S1'), settings());
  assert.equal(seen.every(([method]) => method === 'GET'), true);
  const review = m.review({ ...m.state.imported, minutes: '60' });
  assert.equal(await m.confirm(review), true);
  assert.deepEqual(seen.filter(([method]) => method === 'POST'), [['POST', '/dj/steer', { zone: 'Study@S1', ...settings(), for_secs: 3600 }]]);
  assert.equal(seen.filter(([method, path]) => method === 'GET' && path === '/zones/Study%40S1/dj').length, 3);
});

const rehearsal = require('../crates/fsonos-api/src/remote/dj-session.js');
const previewDraft = () => ({ mood: 'calm', at: '23:00', lengthMode: 'hours', length: '3', seed: '7',
  filters: { include_artists: 'Earth, Wind & Fire\n坂本龍一', exclude_keywords: '<script>not markup</script>',
    min_work_minutes: '2', max_work_minutes: '40', energy_bias: '-1', allow_long_works: true } });
const report = (body) => {
  const from = '2026-10-18T23:00:00-04:00';
  return { zone: 'Study', from, seed: body.seed,
    works: [{ at: from, energy: 35, energy_target: 30, explicit: false, mood: body.mood ?? 'focus',
      work: { composer: 'Example artist', title: 'A complete work', movements: 3, minutes: 12,
        reason: { summary: 'Balanced choice', relaxed: ['keywords'], factors: [] } } }],
    summary: { works: 1, minutes: 12, distinct_artists: 1, explicit: 0,
      hours: [{ from, works: 1, energy: 35, energy_target: 30, artist_entropy: 0, genre_entropy: 0 }] } };
};
function preview(overrides = {}) {
  const calls = [];
  const m = rehearsal.rehearsalModel({ call: async (name, args) => {
    calls.push([name, args]);
    if (overrides.call) return overrides.call(name, args);
    if (name === 'sessionZones') return zones();
    if (name === 'sessionMoods') return { moods: [{ name: 'focus' }, { name: 'calm' }] };
    if (name === 'sessionRehearse') return report(args.body);
    throw new Error(`Unexpected preview call: ${name}`);
  } });
  m.context(zone(overrides.household)); m.open();
  return { m, calls };
}

for (const household of ['S1', 'S2']) test(`a completed ${household} rehearsal hands off only its captured explicit settings`, async () => {
  const { m, calls } = preview({ household });
  assert.equal(m.handoff(), null);
  await m.loadMoods(); const draft = previewDraft();
  assert.equal(await m.generate(draft), true);
  draft.filters.include_artists = 'Edited outside the model'; draft.seed = '88';
  const count = calls.length, handoff = m.handoff();
  assert.deepEqual(handoff, { target: `Study@${household}`, signature: zone(household).signature, settings: settings() });
  assert.equal(calls.length, count, 'preparing a draft performs no network operation');
  assert.throws(() => { handoff.settings.constraints.include_artists[0] = 'Changed'; });
  assert.throws(() => { handoff.target = 'Other@S1'; });
  assert.deepEqual(Object.keys(handoff.settings).sort(), ['constraints', 'mood']);
  assert.deepEqual(handoff.settings.constraints.exclude_keywords, ['<script>not markup</script>'],
    'the requested filter is not removed because the planner relaxed it');
});

test('rehearsals inheriting live steering do not invent a static mood or implicitly clear it', async () => {
  const { m } = preview();
  assert.equal(await m.generate({}), true);
  assert.equal(m.state.report.works[0].mood, 'focus');
  assert.equal(m.handoff(), null);
  await m.generate({ filters: { energy_bias: 0, allow_long_works: false } });
  assert.equal(m.handoff(), null);
});

test('constraints-only previews preserve the absence of a mood in the live draft', async () => {
  const { m } = preview();
  await m.generate({ filters: { include_genres: 'jazz' } });
  assert.deepEqual(m.handoff().settings, { constraints: { include_genres: ['jazz'] } });
  const live = fixture();
  assert.equal(await live.m.openFor(m.handoff(), m.handoff().settings), true);
  assert.deepEqual(live.m.state.imported, m.handoff().settings);
  assert.equal(live.calls.some(([name]) => name === 'djSteeringWrite'), false);
});

test('empty, rejected and malformed rehearsal responses cannot be handed to a live editor', async () => {
  for (const response of ['empty', 'bad', 'error']) {
    const { m } = preview({ call: async (name, args) => {
      if (name === 'sessionZones') return zones();
      if (name === 'sessionMoods') return { moods: [{ name: 'calm' }] };
      if (response === 'error') throw new Error('Planning failed');
      if (response === 'bad') return { wrong: true };
      return { ...report(args.body), works: [], summary: { works: 0, minutes: 0, distinct_artists: 0, explicit: 0, hours: [] } };
    } });
    await m.loadMoods(); await m.generate(previewDraft());
    assert.equal(m.handoff(), null);
  }
});

test('edits and invalid generation attempts cannot pass off old results as the new settings', async () => {
  const { m } = preview(); await m.loadMoods(); await m.generate(previewDraft());
  const before = m.state.report;
  m.edit(); assert.equal(m.handoff(), null); assert.equal(m.state.report, before);
  assert.match(m.state.message, /Settings changed/);
  await m.generate(previewDraft()); assert.ok(m.handoff());
  assert.equal(await m.generate({ mood: 'calm', length: '0' }), false);
  assert.equal(m.handoff(), null);
  await m.generate(previewDraft()); assert.ok(m.handoff());
});

test('pending preflight, planning and postflight responses never expose a draft prematurely', async () => {
  for (const stage of ['preflight', 'plan', 'postflight']) {
    const held = deferred(); let reads = 0;
    const { m, calls } = preview({ call: async (name, args) => {
      if (name === 'sessionMoods') return { moods: [{ name: 'calm' }] };
      if (name === 'sessionZones') {
        reads++;
        if ((stage === 'preflight' && reads === 1) || (stage === 'postflight' && reads === 2)) return held.promise;
        return zones();
      }
      return stage === 'plan' ? held.promise : report(args.body);
    } });
    await m.loadMoods(); const running = m.generate(previewDraft()); await tick();
    assert.equal(m.handoff(), null);
    assert.equal(await m.generate(previewDraft()), false, 'pending planning retains its single request slot');
    held.resolve(stage === 'plan' ? report({ seed: 7, mood: 'calm' }) : zones());
    assert.equal(await running, true); assert.ok(m.handoff());
    assert.equal(calls.filter(([name]) => name === 'sessionRehearse').length, 1);
  }
});

test('editing during a held request leaves the completed report marked as captured, not newly edited', async () => {
  const held = deferred();
  const { m } = preview({ call: async (name) => name === 'sessionZones' ? zones()
    : name === 'sessionMoods' ? { moods: [{ name: 'calm' }] } : held.promise });
  await m.loadMoods(); const running = m.generate(previewDraft()); await tick(); m.edit();
  held.resolve(report({ mood: 'calm', seed: 7 }));
  assert.equal(await running, true);
  assert.equal(m.handoff(), null);
  assert.equal(m.state.edited, true);
});

for (const action of ['close', 'dispose', 'context']) test(`${action} fences a captured rehearsal before it can be handed off`, async () => {
  const { m } = preview(); await m.loadMoods(); await m.generate(previewDraft());
  assert.ok(m.handoff());
  if (action === 'context') m.context(zone('S1')); else m[action]();
  assert.equal(m.handoff(), null);
});

test('a regroup between planning and completion prevents any live draft', async () => {
  let reads = 0;
  const { m } = preview({ call: async (name, args) => name === 'sessionMoods' ? { moods: [{ name: 'calm' }] }
    : name === 'sessionZones' ? (++reads === 1 ? zones() : [{ ...zones()[1], members: ['Study', 'Hall'] }]) : report(args.body) });
  await m.loadMoods(); assert.equal(await m.generate(previewDraft()), false);
  assert.equal(m.handoff(), null);
  assert.match(m.state.message, /group changed/);
});

test('a preview-to-editor flow has no save until explicit review, and a changed group stops that save', async () => {
  const p = preview(); await p.m.loadMoods(); await p.m.generate(previewDraft());
  const transfer = p.m.handoff();
  let current = zones();
  const e = fixture({ call: async (name) => name === 'djSteeringZones' ? current
    : name === 'djSteeringMoods' ? { moods: [{ name: 'calm' }, { name: 'focus' }] } : status() });
  assert.equal(await e.m.openFor(transfer, transfer.settings), true);
  assert.equal(e.calls.some(([name]) => name === 'djSteeringWrite'), false);
  const review = e.m.review({ ...e.m.state.imported, minutes: '90' });
  current = [{ ...zones()[1], members: ['Study', 'Hall'] }];
  assert.equal(await e.m.confirm(review), false);
  assert.equal(e.calls.some(([name]) => name === 'djSteeringWrite'), false);
});

test('real HTTP completes preview-to-steering through the existing endpoints without queue or playback requests', async (t) => {
  const seen = [];
  const server = http.createServer(async (req, res) => {
    let text = ''; for await (const chunk of req) text += chunk;
    const body = text ? JSON.parse(text) : null; seen.push([req.method, req.url, body]);
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.url === '/zones' ? zones() : req.url === '/dj/moods'
      ? { moods: [{ name: 'calm' }, { name: 'focus' }] } : req.url === '/dj/rehearse'
        ? report(body) : req.url === '/dj/steer' ? { done: 'Steering saved', changed: true } : status()));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`, routes = { ...steering.routes, ...rehearsal.routes };
  const call = async (name, { params = {}, body } = {}) => {
    const [method, pattern] = routes[name];
    const path = pattern.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    return (await fetch(base + path, { method, headers: { 'Content-Type': 'application/json' },
      ...(body ? { body: JSON.stringify(body) } : {}) })).json();
  };
  const p = rehearsal.rehearsalModel({ call }); p.context(zone()); p.open(); await p.loadMoods();
  await p.generate(previewDraft()); const handoff = p.handoff();
  const e = steering.model({ call });
  assert.equal(await e.openFor(handoff, handoff.settings), true);
  assert.deepEqual(seen.filter(([method]) => method === 'POST').map(([, path]) => path), ['/dj/rehearse']);
  const review = e.review({ ...e.state.imported, minutes: '45' });
  assert.equal(await e.confirm(review), true);
  assert.deepEqual(seen.filter(([method]) => method === 'POST'), [
    ['POST', '/dj/rehearse', { zone: 'Study@S2', hours: 3, seed: 7, at: '23:00', ...settings() }],
    ['POST', '/dj/steer', { zone: 'Study@S2', ...settings(), for_secs: 2700 }],
  ]);
});
