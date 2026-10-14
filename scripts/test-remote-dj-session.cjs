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
