'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require(process.env.DASHBOARD_CORE || '../crates/fsonos-api/src/remote/dashboard-core.js');
const clone = (v) => structuredClone(v);
const tick = () => new Promise((resolve) => setImmediate(resolve));
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const zone = (name, home = 'S2', members = [name]) => ({ household: home, coordinator_room: name, members, transport_state: 'playing' });
const level = (z, volume = 20) => ({ zone: z, transport_state: 'playing', volume, group_volume: volume, mute: false,
  track: { uri: 'synthetic:song', title: 'A synthetic song', position_secs: 5, duration_secs: 200, queue_position: 1 } });
function fixture(groups = [zone('Alpha'), zone('Beta')]) {
  let time = 1000, hook = null;
  const calls = [], unavailable = new Set();
  const data = { zones: clone(groups), states: new Map(groups.map((z) => [C.key(z), level(z)])) };
  const call = async (url, body) => {
    calls.push({ url, body });
    if (hook) { const answer = hook(url, body); if (answer !== undefined) return answer; }
    if (body !== undefined) return { done: 'Accepted', changed: true };
    if (url === '/zones') return clone(data.zones);
    const target = decodeURIComponent(url.split('/')[2]);
    if (unavailable.has(target)) throw new Error('Synthetic room unavailable');
    if (url.endsWith('/state') && data.states.has(target)) return clone(data.states.get(target));
    throw new Error(`Unexpected ${url}`);
  };
  const model = C.model({ call, now: () => time });
  return { model, data, call, calls, unavailable, time: (t) => { time = t; }, hook: (h) => { hook = h; },
    reads: () => calls.filter((c) => c.url.endsWith('/state')).map((c) => decodeURIComponent(c.url.split('/')[2])),
    writes: () => calls.filter((c) => c.body !== undefined),
    item: (target) => model.state.items.find((i) => C.key(i.zone) === target),
    async ready() { await model.refresh(); calls.length = 0; return this; } };
}

test('member event names resolve every same-named household using qualified group targets', async () => {
  const f = await fixture([zone('Alpha', 'S1', ['Alpha', 'Member']), zone('Alpha', 'S2', ['Alpha', 'Member'])]).ready();
  assert.deepEqual(f.model.eventTargets('Member'), ['Alpha@S1', 'Alpha@S2']);
  assert.equal(f.model.eventTargets('Alpha@S2'), null, 'never guess the household from an unqualified event');
  for (const name of [null, '', 42, 'Unknown']) assert.equal(f.model.eventTargets(name), null);
  f.model.invalidate(); assert.equal(f.model.eventTargets('Member'), null);
});

test('a room-scoped update neither rereads nor restamps unrelated groups', async () => {
  const f = await fixture().ready(), prior = f.item('Beta@S2');
  f.time(9000); f.model.invalidate(['Alpha@S2']);
  assert.equal(prior.fresh, true);
  assert.equal(await f.model.refresh(false, { targets: ['Alpha@S2'] }), true);
  assert.deepEqual(f.reads(), ['Alpha@S2']); assert.equal(f.calls.length, 1);
  assert.equal(f.item('Beta@S2'), prior); assert.equal(prior.at, 1000);
  assert.equal(f.item('Alpha@S2').at, 9000); assert.equal(f.writes().length, 0);
});

test('failure of one target does not remove freshness or control from a healthy selected group', async () => {
  const f = await fixture().ready(); f.model.connection(true); f.unavailable.add('Beta@S2');
  f.model.invalidate(['Beta@S2']); assert.equal(await f.model.refresh(false, { targets: ['Beta@S2'] }), false);
  assert.equal(f.item('Alpha@S2').fresh, true); assert.equal(f.model.canControl(), true);
  assert.deepEqual(f.model.state.failedTargets, ['Beta@S2']); assert.equal(f.model.state.topologyFresh, true);
  assert.match(f.model.state.error, /Some rooms/);
});

test('healthy updates keep failed targets visible instead of claiming household recovery', async () => {
  const f = await fixture().ready(); f.unavailable.add('Beta@S2');
  await f.model.refresh(false, { targets: ['Beta@S2'] }); f.calls.length = 0;
  f.time(2000); assert.equal(await f.model.refresh(false, { targets: ['Alpha@S2'] }), true);
  assert.deepEqual(f.reads(), ['Alpha@S2']); assert.deepEqual(f.model.state.failedTargets, ['Beta@S2']);
  assert.match(f.model.state.error, /Some rooms/); assert.equal(f.item('Beta@S2').at, 1000);
});

test('recovery checks topology then only rereads failed groups', async () => {
  const f = await fixture().ready(); f.unavailable.add('Beta@S2');
  await f.model.refresh(false, { targets: ['Beta@S2'] }); f.calls.length = 0;
  const healthy = f.item('Alpha@S2'); f.unavailable.clear(); f.time(35000);
  assert.equal(await f.model.refresh(false, { recovery: true }), true);
  assert.equal(f.calls[0].url, '/zones'); assert.deepEqual(f.reads(), ['Beta@S2']);
  assert.equal(f.item('Alpha@S2'), healthy); assert.equal(healthy.at, 1000);
  assert.equal(f.item('Beta@S2').at, 35000); assert.deepEqual(f.model.state.failedTargets, []);
  assert.equal(f.model.state.error, '');
});

test('recovery adopts newly discovered groups without re-reading unrelated healthy ones', async () => {
  const f = await fixture().ready(), newZone = zone('Gamma');
  f.data.zones.push(newZone); f.data.states.set('Gamma@S2', level(newZone));
  await f.model.refresh(false, { recovery: true }); assert.deepEqual(f.reads(), ['Gamma@S2']);
  assert.equal(f.model.state.items.length, 3); assert.equal(f.item('Alpha@S2').at, 1000);
});

test('recovery forgets departed failures rather than retrying vanished rooms forever', async () => {
  const f = await fixture().ready(); f.unavailable.add('Beta@S2');
  await f.model.refresh(false, { targets: ['Beta@S2'] }); f.calls.length = 0;
  f.data.zones = [f.data.zones[0]];
  assert.equal(await f.model.refresh(false, { recovery: true }), true);
  assert.deepEqual(f.reads(), []); assert.deepEqual(f.model.state.failedTargets, []);
  assert.equal(f.model.state.items.length, 1); assert.equal(f.model.state.error, '');
});

test('new coordinator and regrouped membership receive new observations during recovery', async () => {
  const f = await fixture([zone('Alpha'), zone('Beta', 'S2', ['Beta', 'Gamma'])]).ready();
  f.unavailable.add('Beta@S2'); await f.model.refresh(false, { targets: ['Beta@S2'] }); f.calls.length = 0;
  const newZone = zone('Gamma', 'S2', ['Beta', 'Gamma']);
  f.data.zones[1] = newZone; f.data.states.set('Gamma@S2', level(newZone, 32));
  await f.model.refresh(false, { recovery: true }); assert.deepEqual(f.reads(), ['Gamma@S2']);
  assert.equal(f.item('Gamma@S2').state.volume, 32); assert.equal(f.item('Beta@S2'), undefined);
  assert.deepEqual(f.model.state.failedTargets, []);
});

test('a changed group under the same coordinator is not mistaken for an unchanged healthy observation', async () => {
  const f = await fixture().ready(), newZone = zone('Alpha', 'S2', ['Alpha', 'Member']);
  f.data.zones[0] = newZone; f.data.states.set('Alpha@S2', level(newZone, 33));
  await f.model.refresh(false, { recovery: true }); assert.deepEqual(f.reads(), ['Alpha@S2']);
  assert.equal(f.item('Alpha@S2').state.volume, 33);
});

test('unknown scoped addresses fall back to topology instead of issuing guessed state requests', async () => {
  const f = await fixture().ready(); await f.model.refresh(false, { targets: ['Missing@S2'] });
  assert.equal(f.calls[0].url, '/zones'); assert.deepEqual(f.reads(), ['Alpha@S2', 'Beta@S2']);
});

test('failed topology requires a full baseline before partial reads can resume', async () => {
  const f = await fixture().ready(); f.hook((url) => { if (url === '/zones') throw new Error('no topology'); });
  assert.equal(await f.model.refresh(false, { recovery: true }), false);
  assert.equal(f.model.state.topologyFresh, false); assert.ok(f.model.state.items.every((i) => !i.fresh));
  f.hook(null); f.calls.length = 0; await f.model.refresh(false, { targets: ['Alpha@S2'] });
  assert.equal(f.calls[0].url, '/zones'); assert.deepEqual(f.reads(), ['Alpha@S2', 'Beta@S2']);
});

test('scoped and recovery requests stay single-flight with full snapshots', async () => {
  const f = await fixture().ready(), d = deferred();
  f.hook((url) => url.endsWith('/state') ? d.promise : undefined);
  const first = f.model.refresh(false, { targets: ['Alpha@S2'] });
  assert.equal(await f.model.refresh(), false); assert.equal(await f.model.refresh(false, { recovery: true }), false);
  assert.equal(await f.model.refresh(false, { targets: ['Beta@S2'] }), false);
  d.resolve(level(f.data.zones[0])); await first; assert.deepEqual(f.reads(), ['Alpha@S2']);
});

test('per-group invalidation rejects its late reply while allowing another group to publish', async () => {
  const f = await fixture().ready(), a = deferred(), b = deferred();
  f.hook((url) => url.includes('Alpha') ? a.promise : url.includes('Beta') ? b.promise : undefined);
  const pending = f.model.refresh(false, { targets: ['Alpha@S2', 'Beta@S2'] });
  f.model.invalidate(['Alpha@S2']); f.time(7000);
  a.resolve(level(f.data.zones[0], 99)); b.resolve(level(f.data.zones[1], 31)); await pending;
  assert.equal(f.item('Alpha@S2').fresh, false); assert.equal(f.item('Alpha@S2').state.volume, 20);
  assert.equal(f.item('Alpha@S2').at, 1000); assert.equal(f.item('Beta@S2').fresh, true);
  assert.equal(f.item('Beta@S2').state.volume, 31); assert.equal(f.item('Beta@S2').at, 7000);
});

test('late failures from an obsolete room read cannot change recovery or server cooldown', async () => {
  const f = await fixture().ready(), d = deferred(); f.hook((url) => url.endsWith('/state') ? d.promise : undefined);
  const pending = f.model.refresh(false, { targets: ['Alpha@S2'] }); f.model.invalidate(['Alpha@S2']);
  d.reject(Object.assign(new Error('old throttling'), { retryAfter: 180000 })); await pending;
  assert.deepEqual(f.model.state.failedTargets, []); assert.equal(f.model.state.retryAfter, 0);
});

test('suspension fences replies and prevents queued reads from starting', async () => {
  const f = await fixture(Array.from({ length: 9 }, (_, n) => zone(`Room${n}`))).ready();
  const gates = []; let peak = 0, ongoing = 0;
  f.hook((url) => {
    if (!url.endsWith('/state')) return;
    const d = deferred(); gates.push(d); peak = Math.max(peak, ++ongoing);
    return d.promise.finally(() => { --ongoing; });
  });
  const pending = f.model.refresh(); await tick(); assert.equal(gates.length, 4);
  f.model.suspend(); for (const d of gates) d.resolve(level(f.data.zones[0])); await pending;
  assert.equal(gates.length, 4); assert.equal(peak, 4); assert.ok(f.model.state.items.every((i) => !i.fresh));
});

test('partial updates have a four-read ceiling across all selected households', async () => {
  const f = await fixture(Array.from({ length: 9 }, (_, n) => zone(`Room${n}`))).ready();
  const gates = []; let peak = 0, ongoing = 0;
  f.hook((url) => {
    const target = decodeURIComponent(url.split('/')[2]); const d = deferred(); gates.push({ d, target });
    peak = Math.max(peak, ++ongoing); return d.promise.finally(() => { --ongoing; });
  });
  const pending = f.model.refresh(false, { targets: f.data.zones.map(C.key) });
  assert.equal(gates.length, 4);
  for (let n = 0; n < 9; ++n) { const { d, target } = gates[n]; d.resolve(f.data.states.get(target)); await tick(); }
  await pending; assert.equal(peak, 4); assert.equal(f.reads().length, 9);
});

test('an event for the selected group invalidates a dependent unsent command', async () => {
  const f = await fixture().ready(), d = deferred();
  const pending = f.model.mutate(async ({ send }) => { await d.promise; return send('/next', { zone: 'Alpha@S2' }); });
  f.model.invalidate(['Alpha@S2']); d.resolve();
  assert.equal(await pending, false); assert.equal(f.writes().length, 0);
});

test('an unrelated group event does not cancel the selected group command preflight', async () => {
  const f = await fixture().ready(), d = deferred();
  const pending = f.model.mutate(async ({ send }) => { await d.promise; return send('/pause', { zone: 'Alpha@S2' }); });
  f.model.invalidate(['Beta@S2']); d.resolve();
  assert.equal(await pending, true); assert.equal(f.writes().length, 1);
});

test('a submitted reply survives group invalidation without command replay', async () => {
  const f = await fixture().ready(), d = deferred(); f.hook((_, body) => body !== undefined ? d.promise : undefined);
  const pending = f.model.command('pause'); await tick(); f.model.invalidate(['Alpha@S2']);
  d.resolve({ done: 'Paused', changed: true }); assert.equal(await pending, true);
  assert.equal(f.model.state.message, 'Paused'); assert.equal(f.writes().length, 1); assert.equal(f.item('Alpha@S2').fresh, false);
});

test('partial and recovery observations cannot clear an uncertain command outcome', async () => {
  const f = await fixture().ready(); f.hook((_, body) => { if (body !== undefined) throw new Error('reply lost'); });
  await f.model.command('next'); f.hook(null);
  await f.model.refresh(false, { targets: ['Alpha@S2'] }); assert.equal(f.model.state.uncertain, true);
  await f.model.refresh(true, { recovery: true }); assert.equal(f.model.state.uncertain, true);
  assert.equal(f.model.canControl(), false); await f.model.refresh(true); assert.equal(f.model.state.uncertain, false);
  assert.equal(f.writes().length, 1);
});

test('malformed group observations remain failed until a valid recovery read', async () => {
  const f = await fixture().ready(); f.data.states.get('Beta@S2').volume = 101;
  await f.model.refresh(false, { targets: ['Beta@S2'] }); assert.deepEqual(f.model.state.failedTargets, ['Beta@S2']);
  assert.equal(f.item('Beta@S2').state.volume, 20); assert.equal(f.item('Alpha@S2').fresh, true);
  f.data.states.get('Beta@S2').volume = 30; await f.model.refresh(false, { recovery: true });
  assert.deepEqual(f.model.state.failedTargets, []); assert.equal(f.item('Beta@S2').state.volume, 30);
});

test('duplicate target requests are read once; invalid scope inputs cause no requests', async () => {
  const f = await fixture().ready(); await f.model.refresh(false, { targets: ['Alpha@S2', 'Alpha@S2'] });
  assert.deepEqual(f.reads(), ['Alpha@S2']); f.calls.length = 0;
  for (const scope of [[], 1, {}, { targets: [] }, { targets: [null] }, { targets: Array(257).fill('Alpha@S2') }]) {
    assert.equal(await f.model.refresh(false, scope), false);
  }
  assert.equal(f.calls.length, 0);
});

test('disposal prevents new scoped or recovery reads and invalidates selections', async () => {
  const f = await fixture().ready(); f.model.dispose();
  assert.equal(await f.model.refresh(false, { targets: ['Alpha@S2'] }), false);
  assert.equal(await f.model.refresh(false, { recovery: true }), false); assert.equal(f.calls.length, 0);
  assert.equal(f.model.eventTargets('Alpha'), null); assert.equal(f.model.canControl(), false);
});

test('real HTTP: an unavailable room recovers without rereading healthy room state', async (t) => {
  const f = fixture(); f.unavailable.add('Beta@S2');
  const server = http.createServer(async (req, res) => {
    try { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(await f.call(req.url))); }
    catch (error) { res.statusCode = 503; res.end(JSON.stringify({ detail: error.message })); }
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  t.after(() => new Promise((r) => { server.close(r); server.closeAllConnections(); }));
  const call = C.request({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) });
  const model = C.model({ call }); assert.equal(await model.refresh(), false); const healthy = model.state.items[0];
  f.calls.length = 0; f.unavailable.clear(); assert.equal(await model.refresh(false, { recovery: true }), true);
  assert.deepEqual(f.reads(), ['Beta@S2']); assert.equal(model.state.items[0], healthy); assert.equal(f.writes().length, 0);
});
