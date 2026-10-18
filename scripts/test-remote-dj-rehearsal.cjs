'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { rehearsalRequest, rehearsalTime, rehearsalOf, rehearsalModel, zonesOf, routes, model: sessionModel } = require('../crates/fsonos-api/src/remote/dj-session.js');

const zones = () => [
  { household: 'S1', coordinator_room: 'Study', members: ['Study', 'Hall'] },
  { household: 'S2', coordinator_room: 'Study', members: ['Study'] },
];
const zone = (target = 'Study@S1') => zonesOf(zones()).find((z) => z.target === target);
const work = () => ({ title: '<Whole work & song>', composer: 'Example artist', movements: 3, minutes: 24,
  performers: ['Ensemble'], album: 'Album', reason: { summary: 'Not heard recently', relaxed: ['keywords'],
    factors: [{ factor: 'feedback', weight: 1250 }, { factor: 'future_factor', weight: -100 }] } });
const report = (extra = {}) => ({ zone: 'Study', from: '2026-10-18T23:40:00-04:00', seed: 1,
  works: [
    { at: '2026-10-18T23:40:00-04:00', work: work(), energy: 35, energy_target: 40, genre: 'jazz', mood: 'focus', explicit: false },
    { at: '2026-10-19T00:04:00-04:00', work: { ...work(), title: 'Next whole work' }, energy: 20, explicit: true },
  ],
  summary: { works: 2, minutes: 48, distinct_artists: 1, explicit: 1,
    hours: [
      { from: '2026-10-18T23:40:00-04:00', works: 1, artist_entropy: 0, genre_entropy: 0, energy: 35, energy_target: 40 },
      { from: '2026-10-19T00:04:00-04:00', works: 1, artist_entropy: 0, genre_entropy: 0, energy: 20 },
    ] }, ...extra });
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const tick = () => new Promise((resolve) => setImmediate(resolve));
const fixture = (extra = {}) => {
  const calls = [];
  const m = rehearsalModel({ now: () => 1234, ...extra, call: async (name, args) => {
    calls.push([name, args]);
    if (extra.call) return extra.call(name, args);
    if (name === 'sessionZones') return zones();
    if (name === 'sessionMoods') return { moods: [{ name: 'focus' }, { name: 'bright' }] };
    if (name === 'sessionRehearse') return report({ seed: args.body.seed });
    throw new Error(`Unexpected endpoint ${name}`);
  } });
  m.context(zone()); m.open();
  return { m, calls };
};

test('request defaults inherit steering, preserve zero seed, and select only one length unit', () => {
  assert.deepEqual(rehearsalRequest(zone()), { zone: 'Study@S1', works: 12, seed: 1 });
  assert.deepEqual(rehearsalRequest(zone('Study@S2'), { lengthMode: 'hours', length: '48', seed: '0', at: ' 23:00 ' }),
    { zone: 'Study@S2', hours: 48, seed: 0, at: '23:00' });
  assert.deepEqual(rehearsalRequest(zone(), { lengthMode: 'hours' }), { zone: 'Study@S1', hours: 2, seed: 1 });
  assert.equal(rehearsalRequest(zone(), { length: 500, seed: Number.MAX_SAFE_INTEGER }).seed, Number.MAX_SAFE_INTEGER);
  assert.equal(rehearsalRequest(zone(), { mood: 'focus' }, ['focus']).mood, 'focus');
  assert.equal(rehearsalRequest(zone(), { at: '   ', mood: '' }).at, undefined);
});

test('invalid limits and unknown options cannot silently change rehearsal intent', () => {
  for (const draft of [null, [], { length: 0 }, { length: 501 }, { length: '-1' }, { length: '1.5' }, { length: '1e2' },
    { lengthMode: 'hours', length: 49 }, { lengthMode: 'minutes' }, { seed: -1 }, { seed: null }, { seed: Infinity },
    { seed: '9007199254740993' }, { seed: Number.MAX_SAFE_INTEGER + 1 }, { seed: '1.1' },
    { mood: 'unknown' }, { mood: false }, { hours: 2 }, { play: true }]) {
    assert.throws(() => rehearsalRequest(zone(), draft, ['focus']), JSON.stringify(draft));
  }
  assert.throws(() => rehearsalRequest(null, {}));
});

test('clock times belong to the house; RFC3339 dates require a real calendar and explicit offset', () => {
  for (const at of ['00:00', '23:59', '2026-10-19T00:04:00-04:00', '2024-02-29T23:00:00Z', '2026-10-18T10:00:00.125+05:30']) {
    assert.equal(rehearsalRequest(zone(), { at }).at, at);
  }
  for (const at of ['24:00', '7:30', '12:60', 'tomorrow', '2026-10-19T00:04:00', '2026-02-29T10:00:00Z',
    '2026-04-31T00:00:00Z', '2026-00-01T00:00:00Z', '2026-13-01T00:00:00Z', '2026-01-00T00:00:00Z', false]) {
    assert.throws(() => rehearsalRequest(zone(), { at }), String(at));
  }
  assert.equal(rehearsalTime(null), false);
});

test('response preserves date boundaries, complete works, reasons and optional fields without sharing mutable data', () => {
  const raw = report();
  const value = rehearsalOf(raw, zone(), rehearsalRequest(zone()));
  raw.works[0].work.title = 'Mutated';
  assert.equal(value.works[0].work.title, '<Whole work & song>');
  assert.equal(value.works[0].work.movements, 3);
  assert.equal(value.works[1].at, '2026-10-19T00:04:00-04:00');
  assert.equal(value.works[0].work.reason.factors[1].weight, -100);
  assert.deepEqual(value.works[0].work.reason.relaxed, ['keywords']);
  assert.equal(value.works[1].energy_target, undefined);
  assert.equal(value.summary.explicit, 1);
});

test('unreadable or mismatched rehearsal responses are rejected as a whole', () => {
  const mutations = [
    (r) => { r.zone = 'Other'; }, (r) => { r.seed = 2; }, (r) => { r.seed = Number.MAX_SAFE_INTEGER + 1; },
    (r) => { r.from = 'tomorrow'; }, (r) => { r.works = [null]; }, (r) => { r.works[0].work.movements = 0; },
    (r) => { r.works[0].energy = 101; }, (r) => { r.works[0].energy_target = -1; },
    (r) => { r.works[0].explicit = 'false'; }, (r) => { r.works[0].at = '2026-10-18T20:00:00-04:00'; },
    (r) => { r.works[1].at = r.works[0].at; }, (r) => { r.summary.works = 3; },
    (r) => { r.summary.minutes = -1; }, (r) => { r.summary.explicit = 0; },
    (r) => { r.summary.distinct_artists = 3; }, (r) => { r.summary.hours = []; },
    (r) => { r.summary.hours[0].works = 0; }, (r) => { r.summary.hours[0].artist_entropy = NaN; },
    (r) => { r.summary.hours[0].genre_entropy = -1; }, (r) => { r.summary.hours[0].energy = 101; },
  ];
  for (const mutate of mutations) { const value = report(); mutate(value); assert.throws(() => rehearsalOf(value, zone(), rehearsalRequest(zone()))); }
  for (const value of [null, {}, []]) assert.throws(() => rehearsalOf(value, zone(), rehearsalRequest(zone())));
  assert.throws(() => rehearsalOf(report(), zone(), rehearsalRequest(zone(), { length: '1' })));
  assert.throws(() => rehearsalOf(report(), zone(), rehearsalRequest(zone(), { at: '23:00' })));
  assert.throws(() => rehearsalOf(report(), zone(), rehearsalRequest(zone(), { at: '2026-10-19T23:40:00-04:00' })));
});

test('empty and shorter sets are valid; the 500-work ceiling applies to hour-based requests', () => {
  const empty = report({ works: [], summary: { works: 0, minutes: 0, distinct_artists: 0, explicit: 0, hours: [] } });
  assert.equal(rehearsalOf(empty, zone(), rehearsalRequest(zone())).works.length, 0);
  assert.equal(rehearsalOf(report(), zone(), rehearsalRequest(zone(), { length: '100' })).works.length, 2);
  const full = report();
  full.works = Array.from({ length: 500 }, () => ({ ...report().works[0], explicit: false }));
  full.summary = { works: 500, minutes: 12000, distinct_artists: 1, explicit: 0,
    hours: [{ ...report().summary.hours[0], works: 500 }] };
  assert.equal(rehearsalOf(full, zone(), rehearsalRequest(zone(), { lengthMode: 'hours' })).works.length, 500);
  full.works.push(full.works[0]);
  assert.throws(() => rehearsalOf(full, zone(), rehearsalRequest(zone(), { lengthMode: 'hours' })));
});

for (const generation of ['S1', 'S2']) test(`rehearses the explicitly observed ${generation} group with one POST and no playback operations`, async () => {
  const { m, calls } = fixture(); m.context(zone(`Study@${generation}`));
  await m.loadMoods();
  assert.equal(await m.generate({ lengthMode: 'hours', length: '2', mood: 'focus', seed: '0', at: '23:40' }), true);
  assert.deepEqual(calls.map(([name]) => name), ['sessionMoods', 'sessionZones', 'sessionRehearse', 'sessionZones']);
  assert.deepEqual(calls[2][1], { body: { zone: `Study@${generation}`, hours: 2, seed: 0, at: '23:40', mood: 'focus' } });
  assert.equal(m.state.generatedAt, 1234);
  assert.equal(m.state.report.seed, 0);
  assert.match(m.state.message, /Nothing was played/);
});

test('opening a rehearsal sends nothing and requires an explicit current group', async () => {
  const calls = [];
  const m = rehearsalModel({ call: async (...args) => { calls.push(args); } });
  m.open(); assert.equal(await m.generate({}), false); m.close(); m.context(zone());
  assert.equal(await m.generate({}), false); assert.deepEqual(calls, []);
});

test('group changes before dispatch or before display prevent misattributed plans', async () => {
  for (const changedRead of [1, 2]) {
    let reads = 0;
    const { m, calls } = fixture({ call: async (name) => name === 'sessionZones'
      ? (++reads === changedRead ? [{ ...zones()[0], members: ['Study'] }] : zones()) : report() });
    assert.equal(await m.generate({}), false);
    assert.equal(m.state.report, null);
    assert.match(m.state.message, /group changed/);
    assert.equal(calls.filter(([name]) => name === 'sessionRehearse').length, changedRead === 1 ? 0 : 1);
  }
});

test('mere membership ordering does not invalidate the same group', async () => {
  const { m } = fixture({ call: async (name) => name === 'sessionZones'
    ? [{ ...zones()[0], members: ['Hall', 'Study'] }, zones()[1]] : report() });
  assert.equal(await m.generate({}), true);
});

for (const change of ['close', 'context', 'dispose']) test(`${change} discards a late rehearsal but retains its occupied request slot`, async () => {
  const delayed = deferred();
  const { m, calls } = fixture({ call: async (name) => name === 'sessionZones' ? zones() : delayed.promise });
  const old = m.generate({}); await tick();
  assert.equal(m.state.busy, true);
  if (change === 'context') m.context(zone('Study@S2')); else m[change]();
  m.open(); assert.equal(await m.generate({}), false);
  assert.equal(calls.filter(([name]) => name === 'sessionRehearse').length, 1);
  delayed.resolve(report());
  assert.equal(await old, false); assert.equal(m.state.report, null); assert.equal(m.state.busy, false);
});

test('closing during preflight prevents the POST completely', async () => {
  const delayed = deferred();
  const { m, calls } = fixture({ call: async () => delayed.promise });
  const old = m.generate({}); m.close(); delayed.resolve(zones());
  assert.equal(await old, false);
  assert.deepEqual(calls.map(([name]) => name), ['sessionZones']);
});

test('double-submit coalesces to one request and does not mutate its captured body', async () => {
  const delayed = deferred();
  const { m, calls } = fixture({ call: async (name) => name === 'sessionZones' ? zones() : delayed.promise });
  const draft = { length: '2', seed: '1' }; const pending = m.generate(draft); draft.length = '200';
  await tick(); assert.equal(await m.generate({}), false);
  assert.equal(calls[1][1].body.works, 2);
  assert.equal(Object.isFrozen(calls[1][1].body), true);
  delayed.resolve(report()); assert.equal(await pending, true);
});

test('uncertain transport errors never imply music changed and never trigger an automatic retry', async () => {
  const { m, calls } = fixture({ call: async (name) => {
    if (name === 'sessionZones') return zones();
    throw Object.assign(new Error('No reply'), { uncertain: true, hint: 'It may have played.' });
  } });
  assert.equal(await m.generate({}), false);
  assert.equal(m.state.report, null); assert.equal(m.state.busy, false);
  assert.match(m.state.message, /does not play or queue/);
  assert.doesNotMatch(m.state.message, /may have played/);
  assert.equal(calls.filter(([name]) => name === 'sessionRehearse').length, 1);
});

test('policy errors remain actionable and invalid input never reaches the daemon', async () => {
  const { m, calls } = fixture({ call: async (name) => {
    if (name === 'sessionZones') return zones();
    throw Object.assign(new Error('POLICY_DENIED'), { hint: 'Allow dj_rehearse for this client.' });
  } });
  assert.equal(await m.generate({ seed: 'bad' }), false); assert.deepEqual(calls, []);
  assert.equal(await m.generate({}), false); assert.match(m.state.message, /Allow dj_rehearse/);
});

test('mood errors and ambiguous catalog entries still allow inherited steering; reads do not overlap', async () => {
  for (const value of [null, { moods: null }, { moods: [{ name: '' }] }, { moods: [{ name: 'focus' }, { name: 'focus' }] }]) {
    const { m } = fixture({ call: async (name) => name === 'sessionMoods' ? value : name === 'sessionZones' ? zones() : report() });
    assert.equal(await m.loadMoods(), false); assert.match(m.state.moodError, /Current steering is still available/);
    assert.equal(await m.generate({}), true);
  }
  const delayed = deferred(); const { m, calls } = fixture({ call: async () => delayed.promise });
  const first = m.loadMoods(); assert.equal(await m.loadMoods(), false); assert.equal(calls.length, 1);
  m.dispose(); delayed.resolve({ moods: [{ name: 'focus' }] }); assert.equal(await first, false);
});

test('inspector remains usable for a stopped DJ and does not start a rehearsal on selection', async () => {
  const calls = [];
  const m = sessionModel({ call: async (name) => {
    calls.push(name); return name === 'sessionZones' ? zones() : { zone: 'Study', running: false, steering: { source: 'none' } };
  } });
  await m.refresh(); await m.select('Study@S2');
  assert.equal(m.state.status.running, false); assert.equal(m.state.stale, false);
  assert.deepEqual(calls, ['sessionZones', 'sessionStatus']);
});

test('real loopback JSON uses the public rehearsal route with literal Unicode room identity', async (t) => {
  const seen = [], room = 'Étude / 夜 & Hall';
  const observed = [{ household: 'S2', coordinator_room: room, members: [room] }];
  const server = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    seen.push({ method: req.method, url: req.url, type: req.headers['content-type'], body: body ? JSON.parse(body) : null });
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.url === '/zones' ? observed : report({ zone: room, seed: JSON.parse(body).seed })));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const m = rehearsalModel({ call: async (name, { body } = {}) => {
    const [method, path] = routes[name];
    const response = await fetch(`http://127.0.0.1:${server.address().port}${path}`, {
      method, headers: { Accept: 'application/json', ...(body ? { 'Content-Type': 'application/json' } : {}) },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    return response.json();
  } });
  m.context(zonesOf(observed)[0]); m.open();
  assert.equal(await m.generate({ length: '2', seed: '42' }), true);
  assert.deepEqual(seen.map(({ method, url }) => [method, url]), [['GET', '/zones'], ['POST', '/dj/rehearse'], ['GET', '/zones']]);
  assert.deepEqual(seen[1].body, { zone: `${room}@S2`, works: 2, seed: 42 });
  assert.equal(seen[1].type, 'application/json'); assert.equal(m.state.report.zone, room);
});
