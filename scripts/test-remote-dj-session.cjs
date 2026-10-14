'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { model, zonesOf, statusOf, constraintsText, routes } = require('../crates/fsonos-api/src/remote/dj-session.js');
const zones = () => [
  { household: 'S1', coordinator_room: 'Study', members: ['Study', 'Hall'] },
  { household: 'S2', coordinator_room: 'Study', members: ['Study'] },
];
const work = () => ({ composer: 'Example artist', title: '<A song & a work>', performers: ['Ensemble'],
  album: 'Album', movements: 3, movement: 2, movement_title: 'Andante', minutes: 24,
  reason: { summary: 'Not heard recently', factors: [{ factor: 'feedback', weight: 1250 }], relaxed: ['keywords'] } });
const status = () => ({ zone: 'Study', running: true, now: work(), next: [work(), work()],
  steering: { source: 'session', mood: 'focus', constraints: { include_genres: ['jazz'], energy_bias: -1 },
    added: { include_artists: ['Earth, Wind & Fire'] }, expires_at: 1700001000, expires_in_secs: 600 },
  problem: 'Last queue top-up failed; the daemon will try again.' });
const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
const fixture = (extra = {}) => model({ call: async (name) => name === 'sessionZones' ? zones() : status(), ...extra });

for (const generation of ['S1', 'S2']) test(`reads the explicitly selected ${generation} group, never a default room`, async () => {
  const calls = [];
  const m = fixture({ now: () => 1234, call: async (name, args) => { calls.push([name, args]); return name === 'sessionZones' ? zones() : status(); } });
  assert.equal(await m.refresh(), true);
  assert.equal(m.state.target, '');
  assert.deepEqual(calls, [['sessionZones', undefined]]);
  await m.select(`Study@${generation}`);
  assert.deepEqual(calls[1], ['sessionStatus', { params: { room: `Study@${generation}` } }]);
  assert.equal(m.state.observedAt, 1234);
  assert.equal(m.state.status.now.movement, 2);
  assert.equal(m.state.status.next.length, 2);
  assert.equal(m.state.status.problem, status().problem);
});

test('preserves reasons, relaxed filters, empty optional fields and unknown factor names', () => {
  const value = status();
  value.now.reason.factors.push({ factor: 'future_factor', weight: -100 });
  const parsed = statusOf(value, zonesOf(zones())[0]);
  value.now.title = 'Changed externally';
  assert.equal(parsed.now.title, '<A song & a work>');
  assert.equal(parsed.now.reason.factors[1].weight, -100);
  assert.equal(statusOf({ zone: 'Study', running: false, steering: { source: 'none' } }, zonesOf(zones())[0]).running, false);
});

test('rejects ambiguous rooms and incomplete or cross-coordinator observations', () => {
  for (const value of [null, {}, [zones()[0], zones()[0]], [{ ...zones()[0], members: ['Hall'] }],
    [{ ...zones()[0], members: ['Study', 'Study'] }]]) assert.throws(() => zonesOf(value));
  const bad = [null, {}, { ...status(), zone: 'Other' }, { ...status(), running: 'yes' },
    { ...status(), next: [{}] }, { ...status(), steering: [] }];
  for (const value of bad) assert.throws(() => statusOf(value, zonesOf(zones())[0]));
  for (const change of [{ movements: 0 }, { movement: 4 }, { reason: { summary: 'x', factors: [{ weight: NaN }] } }]) {
    assert.throws(() => statusOf({ ...status(), now: { ...work(), ...change } }, zonesOf(zones())[0]));
  }
});

test('new group selection defeats a late reply for the old group', async () => {
  const delayed = deferred();
  const m = fixture({ call: async (name, args) => name === 'sessionZones' ? zones()
    : args.params.room === 'Study@S1' ? delayed.promise : { ...status(), running: false } });
  await m.refresh();
  const first = m.select('Study@S1');
  await m.select('Study@S2');
  delayed.resolve(status());
  assert.equal(await first, false);
  assert.equal(m.state.target, 'Study@S2');
  assert.equal(m.state.status.running, false);
});

test('closing and disposal discard outstanding reads without starting more', async () => {
  for (const stop of ['close', 'dispose']) {
    const delayed = deferred(); let reads = 0;
    const m = fixture({ call: async (name) => { reads++; return name === 'sessionZones' ? zones() : delayed.promise; } });
    await m.refresh();
    const reading = m.select('Study@S1');
    m[stop]();
    delayed.resolve(status());
    assert.equal(await reading, false);
    assert.equal(m.state.status, null);
    assert.equal(m.state.open, false);
    assert.equal(reads, 2);
    if (stop === 'dispose') assert.equal(await m.refresh(), false);
  }
});

test('refreshing retains unchanged membership but invalidates a regrouped destination', async () => {
  let current = zones(); let reads = 0;
  const m = fixture({ call: async (name) => name === 'sessionZones' ? current : (reads++, status()) });
  await m.refresh(); await m.select('Study@S1');
  current[0].members.reverse();
  await m.refresh();
  assert.equal(m.state.target, 'Study@S1');
  current[0].members = ['Study'];
  await m.refresh();
  assert.equal(m.state.target, '');
  assert.equal(m.state.status, null);
  assert.match(m.state.message, /group changed/);
  assert.equal(reads, 2);
});

test('failed reads retain visibly stale observations and actionable errors', async () => {
  let fail = false;
  const m = fixture({ call: async (name) => {
    if (fail) throw Object.assign(new Error('Unavailable'), { hint: 'Check the daemon.' });
    return name === 'sessionZones' ? zones() : status();
  } });
  await m.refresh(); await m.select('Study@S1');
  fail = true;
  assert.equal(await m.refresh(), false);
  assert.equal(m.state.stale, true);
  assert.equal(m.state.status.now.title, work().title);
  assert.equal(m.state.message, 'Unavailable Check the daemon.');
  fail = false; await m.refresh();
  assert.equal(m.state.stale, false);
});

test('a new refresh wins over an old failed refresh', async () => {
  const delayed = deferred(); let reads = 0;
  const m = fixture({ call: async () => ++reads === 1 ? delayed.promise : zones() });
  const old = m.refresh();
  await m.refresh();
  delayed.resolve({ bad: true });
  assert.equal(await old, false);
  assert.equal(m.state.zones.length, 2);
  assert.equal(m.state.message, 'Choose a group to inspect its DJ.');
});

test('filter explanations preserve literal names and do not hide unfamiliar fields', () => {
  assert.deepEqual(constraintsText({ include_artists: ['Earth, Wind & Fire', '<Example>'], energy_bias: 0,
    periods: [], allow_long_works: false, future: { setting: 'value' } }),
  ['Only artists: Earth, Wind & Fire; <Example>', 'future: {"setting":"value"}']);
});

test('real loopback HTTP reads use the shipped routes and preserve the complete response', async (t) => {
  const seen = [];
  const server = http.createServer((req, res) => {
    seen.push([req.method, req.url]);
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.url === '/zones' ? zones() : status()));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`;
  const m = model({ call: async (name, { params = {} } = {}) => {
    const [method, template] = routes[name];
    const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    return (await fetch(base + path, { method })).json();
  } });
  await m.refresh(); await m.select('Study@S2'); m.close();
  assert.deepEqual(seen, [['GET', '/zones'], ['GET', '/zones/Study%40S2/dj']]);
  assert.deepEqual(m.state.status, status());
});

const steering = require('../crates/fsonos-api/src/remote/dj-steering.js');
const direction = (extra = {}) => {
  const calls = [];
  const m = steering.model({ ...extra, call: async (name, args) => {
    calls.push([name, args]);
    if (extra.call) return extra.call(name, args);
    return name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }] }
      : name === 'djSteeringWrite' ? { done: 'Saved', changed: true, notes: [{ code: 'INFO', detail: 'Next pick only.' }] } : status();
  } });
  return { m, calls };
};
const handoff = (generation = 'S2') => zonesOf(zones()).find((z) => z.target === `Study@${generation}`);

for (const generation of ['S1', 'S2']) test(`inspector hands the ${generation} group to the single editor without a write`, async () => {
  const { m, calls } = direction();
  assert.equal(await m.openFor(handoff(generation)), true);
  assert.equal(m.state.target, `Study@${generation}`);
  assert.deepEqual(calls.at(-1), ['djSteeringStatus', { params: { room: `Study@${generation}` } }]);
  assert.equal(calls.some(([name]) => name === 'djSteeringWrite'), false);
});

test('handoff checks the complete membership and never substitutes a new group', async () => {
  const { m, calls } = direction({ call: async (name) => name === 'djSteeringZones' ? [{ ...zones()[0], members: ['Study'] }]
    : { moods: [{ name: 'focus' }] } });
  assert.equal(await m.openFor(handoff('S1')), false);
  assert.equal(m.state.target, '');
  assert.equal(m.state.ready, false);
  assert.match(m.state.message, /inspected group changed/);
  assert.equal(calls.some(([name]) => name === 'djSteeringStatus' || name === 'djSteeringWrite'), false);
});

test('a later handoff wins over a delayed older handoff; closing invalidates reads', async () => {
  const delayed = deferred(); let reads = 0;
  const { m } = direction({ call: async (name) => name === 'djSteeringZones' ? (++reads === 1 ? delayed.promise : zones())
    : name === 'djSteeringMoods' ? { moods: [{ name: 'focus' }] } : status() });
  const old = m.openFor(handoff('S1'));
  assert.equal(await m.openFor(handoff('S2')), true);
  delayed.resolve(zones());
  assert.equal(await old, false);
  assert.equal(m.state.target, 'Study@S2');
  const wait = deferred();
  const closing = direction({ call: async (name) => name === 'djSteeringZones' ? wait.promise : { moods: [] } }).m;
  const pending = closing.openFor(handoff()); closing.close(); wait.resolve(zones());
  assert.equal(await pending, false);
  assert.equal(closing.state.open, false);
});

test('handoff requires an explicit valid destination and does not interrupt a sent mutation', async () => {
  const wait = deferred();
  const { m, calls } = direction({ call: async (name) => name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods'
    ? { moods: [{ name: 'focus' }] } : name === 'djSteeringWrite' ? wait.promise : status() });
  for (const bad of [null, {}, { target: '', signature: 'x' }, { target: 'Study@S2' }]) assert.equal(await m.openFor(bad), false);
  assert.equal(calls.length, 0);
  await m.openFor(handoff()); const review = m.review({ mood: 'focus', keep_expiry: true });
  const saving = m.confirm(review); await new Promise((resolve) => setImmediate(resolve));
  assert.equal(await m.openFor(handoff('S1')), false);
  assert.equal(m.state.target, 'Study@S2');
  wait.resolve({ done: 'Saved', changed: true }); assert.equal(await saving, true);
});

test('preserving expiry uses the new server countdown rather than the old duration or phone clock', async () => {
  let remaining = 600;
  const { m, calls } = direction({ call: async (name) => name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods'
    ? { moods: [{ name: 'focus' }] } : name === 'djSteeringWrite' ? { done: 'Saved', changed: true }
      : { ...status(), steering: { ...status().steering, expires_in_secs: remaining } } });
  await m.openFor(handoff());
  const review = m.review({ mood: 'focus', constraints: { include_artists: ['Earth, Wind & Fire'] }, keep_expiry: true });
  assert.equal(review.keep_expiry, true);
  assert.equal(review.deadline, status().steering.expires_at);
  assert.equal(review.body.for_secs, undefined);
  remaining = 179;
  assert.equal(await m.confirm(review), true);
  assert.deepEqual(calls.find(([name]) => name === 'djSteeringWrite')[1].body,
    { zone: 'Study@S2', mood: 'focus', constraints: { include_artists: ['Earth, Wind & Fire'] }, for_secs: 179 });
  assert.equal(review.body.for_secs, undefined, 'the immutable reviewed body is not mutated');
});

test('deadline preservation rejects absent deadlines, short countdowns and conflicting new durations', async () => {
  for (const patch of [{ source: 'program' }, { expires_at: null }, { expires_in_secs: null },
    { expires_in_secs: 59 }, { expires_in_secs: 604801 }]) {
    const { m } = direction({ call: async (name) => name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods'
      ? { moods: [{ name: 'focus' }] } : { ...status(), steering: { ...status().steering, ...patch } } });
    await m.openFor(handoff());
    assert.equal(m.review({ mood: 'focus', keep_expiry: true }), null);
  }
  for (const draft of [{ mood: 'focus', keep_expiry: 'true' }, { mood: 'focus', keep_expiry: true, minutes: '30' }]) {
    assert.throws(() => steering.requestOf('Study@S2', draft, ['focus']));
  }
});

test('a deadline that expires during review fails before writing and never gets an implicit extension', async () => {
  let remaining = 600;
  const { m, calls } = direction({ call: async (name) => name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods'
    ? { moods: [{ name: 'focus' }] } : { ...status(), steering: { ...status().steering, expires_in_secs: remaining } } });
  await m.openFor(handoff()); const review = m.review({ mood: 'focus', keep_expiry: true });
  remaining = 59;
  assert.equal(await m.confirm(review), false);
  assert.equal(calls.some(([name]) => name === 'djSteeringWrite'), false);
  assert.match(m.state.message, /deadline is too close/);
  assert.equal(m.state.ready, false);
});

test('new duration, indefinite steering and clear retain the established API contract and notes', async () => {
  for (const [draft, expected] of [[{ mood: 'focus', minutes: '30' }, { zone: 'Study@S2', mood: 'focus', constraints: {}, for_secs: 1800 }],
    [{ mood: 'focus' }, { zone: 'Study@S2', mood: 'focus', constraints: {} }], [{ clear: true, keep_expiry: true }, { zone: 'Study@S2', clear: true }]]) {
    const { m, calls } = direction(); await m.openFor(handoff());
    assert.equal(await m.confirm(m.review(draft)), true);
    assert.deepEqual(calls.find(([name]) => name === 'djSteeringWrite')[1].body, expected);
    assert.equal(m.state.message, 'Saved Next pick only.');
    assert.equal(m.state.ready, true);
  }
});

test('keep-expiry saves remain single dispatch and uncertain answers are never replayed', async () => {
  const wait = deferred(); let writes = 0;
  const { m } = direction({ call: async (name) => name === 'djSteeringZones' ? zones() : name === 'djSteeringMoods'
    ? { moods: [{ name: 'focus' }] } : name === 'djSteeringWrite' ? (writes++, wait.promise) : status() });
  await m.openFor(handoff()); const reviewed = m.review({ mood: 'focus', keep_expiry: true });
  const first = m.confirm(reviewed);
  assert.equal(await m.confirm(reviewed), false);
  wait.resolve({}); assert.equal(await first, false);
  assert.equal(writes, 1);
  assert.equal(await m.confirm(reviewed), false);
  assert.match(m.state.message, /Outcome uncertain/);
});

test('real localhost handoff and preserved deadline dispatch the same API used by both panels', async (t) => {
  const seen = []; let remaining = 600;
  const server = http.createServer(async (req, res) => {
    let bytes = ''; for await (const chunk of req) bytes += chunk;
    seen.push({ method: req.method, path: req.url, body: bytes ? JSON.parse(bytes) : null });
    const result = req.url === '/zones' ? zones() : req.url === '/dj/moods' ? { moods: [{ name: 'focus' }] }
      : req.method === 'POST' ? { done: 'Saved', changed: true }
        : { ...status(), steering: { ...status().steering, expires_in_secs: remaining } };
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(result));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const apiRoutes = { ...routes, ...steering.routes };
  const call = async (name, { params = {}, body } = {}) => {
    const [method, template] = apiRoutes[name];
    const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    return (await fetch(`http://127.0.0.1:${server.address().port}${path}`,
      { method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined })).json();
  };
  const inspect = model({ call }); await inspect.refresh(); await inspect.select('Study@S2');
  const editor = steering.model({ call }); await editor.openFor(inspect.state.zones.find((z) => z.target === inspect.state.target));
  const review = editor.review({ mood: 'focus', keep_expiry: true }); remaining = 120;
  assert.equal(await editor.confirm(review), true);
  assert.deepEqual(seen.filter((r) => r.method === 'POST'), [{ method: 'POST', path: '/dj/steer',
    body: { zone: 'Study@S2', mood: 'focus', constraints: {}, for_secs: 120 } }]);
});
