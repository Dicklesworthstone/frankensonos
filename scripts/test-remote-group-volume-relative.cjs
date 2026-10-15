// Relative native group volume and cross-card observation ordering.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const Group = require('../crates/fsonos-api/src/remote/group-volume.js');
const zone = { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Office'] };
const state = (volume = 20, group = zone) => ({ zone: group, group_volume: volume, volume: 9, mute: true, transport_state: 'paused' });
const keys = zone.members.map(r => JSON.stringify([zone.household, r]));
function deferred() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
function setup(options = {}) {
  const calls = []; let read = () => state(), post = () => ({ done: 'Group adjusted', changed: true, volume: 22 });
  const registry = options.registry || Group.recovery();
  const api = Group.model({ zone, registry, call: async (name, args) => {
    calls.push([name, structuredClone(args)]); return name === 'state' ? read() : post();
  }, ...options });
  api.observe(state(20, options.zone || zone));
  return { api, calls, registry, read: fn => { read = fn; }, post: fn => { post = fn; } };
}

test('quieter and louder remain native relative commands, not invented absolute targets', async () => {
  for (const delta of [-100, -5, -1, 1, 5, 100]) {
    const h = setup(); const selected = h.api.nudge(delta);
    assert.ok(Object.isFrozen(selected)); assert.equal(h.calls.length, 0);
    assert.equal(await h.api.apply(selected), true);
    assert.deepEqual(h.calls[1], ['volume', { body: { zone: 'Den@S2', group: true, delta } }]);
    assert.deepEqual(h.calls.map(([name]) => name), ['state', 'volume', 'state']);
  }
});
test('near a boundary the daemon receives the actual relative request to cap, not client arithmetic', async () => {
  const h = setup(); h.api.observe(state(98)); h.read(() => state(98));
  await h.api.apply(h.api.nudge(5)); assert.equal(h.calls[1][1].body.delta, 5);
  assert.equal(h.calls[1][1].body.volume, undefined);
});
test('invalid deltas consume previous reviews without making requests', async () => {
  for (const delta of [0, 101, -101, 0.5, '5', null, undefined, Infinity, NaN]) {
    const h = setup(), selected = h.api.nudge(5); assert.equal(h.api.nudge(delta), null);
    assert.equal(await h.api.apply(selected), false); assert.equal(h.calls.length, 0);
  }
});
test('group zero and maximum reject impossible directions but allow reversal', () => {
  const h = setup(); h.api.observe(state(0)); assert.equal(h.api.nudge(-5), null); assert.ok(h.api.nudge(5));
  h.api.observe(state(100)); assert.equal(h.api.nudge(5), null); assert.ok(h.api.nudge(-5));
  assert.equal(h.calls.length, 0);
});
test('repeated previews replace rather than accumulate a queue of relative changes', async () => {
  const h = setup(); let first, last;
  for (let n = 0; n < 100; n++) { last = h.api.nudge(5); first ||= last; }
  assert.equal(h.calls.length, 0); assert.equal(last.delta, 5);
  assert.equal(await h.api.apply(first), false); await h.api.apply(last);
  assert.equal(h.calls.filter(([name]) => name === 'volume').length, 1);
});
test('absolute and relative reviews cannot be interchanged or revived', async () => {
  const h = setup(), absolute = h.api.preview(40), relative = h.api.nudge(-5);
  assert.equal(await h.api.apply(absolute), false);
  const newAbsolute = h.api.preview(30); assert.equal(await h.api.apply(relative), false);
  h.api.cancelReview(); assert.equal(await h.api.apply(newAbsolute), false); assert.equal(h.calls.length, 0);
});
test('changed volume preflight rejects a relative review without sending any mutation', async () => {
  const h = setup(); h.read(() => state(30));
  assert.equal(await h.api.apply(h.api.nudge(-5)), false);
  assert.deepEqual(h.calls.map(([name]) => name), ['state']);
  assert.equal(h.api.view().result.status, 'not_sent');
});
test('a clamped relative result does not automatically repeat the delta', async () => {
  const h = setup(); h.post(() => ({ done: 'Group limited', changed: true, volume: 21, notes: [{ detail: 'Room cap limits the adjustment.' }] }));
  assert.equal(await h.api.apply(h.api.nudge(5)), true); assert.match(h.api.view().result.detail, /Room cap/);
  h.api.observe(state(21)); assert.equal(h.api.view().review, null);
  assert.equal(h.calls.filter(([name]) => name === 'volume').length, 1);
});
test('lost relative replies survive passive reads and are explicitly reconciled without replay', async () => {
  const h = setup(); h.post(() => { throw new Error('lost response'); });
  assert.equal(await h.api.apply(h.api.nudge(-5)), false); h.api.observe(state(15));
  assert.equal(h.api.nudge(-5), null); h.read(() => state(15));
  assert.equal(await h.api.refresh(), true); assert.equal(h.api.view().result.status, 'reconciled');
  assert.match(h.api.view().result.detail, /Current group level is 15/);
  assert.equal(h.calls.filter(([name]) => name === 'volume').length, 1);
});
test('another group operation fences an explicit read that started before it', async () => {
  const h = setup(), read = deferred(); h.read(() => read.promise);
  const loading = h.api.refresh(); const operation = h.registry.start(keys);
  read.resolve(state(80)); assert.equal(await loading, false);
  assert.equal(h.api.view().value, 20); assert.equal(h.api.view().fresh, false);
  h.registry.finish(operation, false); assert.equal(h.api.view().ready, false);
  h.api.observe(state(30)); assert.equal(h.api.view().ready, true);
});
test('both start and completion retire other cards review snapshots', async () => {
  const h = setup(), selected = h.api.nudge(5), operation = h.registry.start(keys);
  assert.equal(h.api.view().review, null); assert.equal(await h.api.apply(selected), false);
  assert.equal(h.api.observe(state(30)), false);
  h.registry.finish(operation, false); assert.equal(h.api.view().fresh, false);
  assert.equal(h.api.nudge(5), null); h.api.observe(state(30)); assert.ok(h.api.nudge(5));
});
test('operations on another household do not invalidate a ready group or its draft', () => {
  const h = setup(), selected = h.api.nudge(5);
  const operation = h.registry.start(zone.members.map(r => JSON.stringify(['S1', r])));
  assert.equal(h.api.view().ready, true); assert.equal(h.api.view().review, selected);
  h.registry.finish(operation, true); assert.equal(h.api.view().ready, true);
});
test('reconciliations cannot clear a still-pending operation', () => {
  const r = Group.recovery(), operation = r.start(keys); r.clear(keys);
  assert.equal(r.status(keys).pending, true); assert.equal(r.start(keys), null);
  r.finish(operation, true); assert.equal(r.status(keys).uncertain, true);
  r.clear([keys[0]]); assert.equal(r.status([keys[1]]).uncertain, true);
});

test('real HTTP relative volume sends a single delta and no playback or room-level side commands', async t => {
  const requests = []; let groupVolume = 20;
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null;
    requests.push([req.method, req.url, body]);
    if (body) { assert.equal(body.group, true); assert.equal(body.volume, undefined); groupVolume += body.delta; }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(body ? { done: 'Group adjusted', changed: true, volume: groupVolume } : state(groupVolume)));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening'); t.after(() => { server.closeAllConnections(); server.close(); });
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = setup({ call: async (name, args) => {
    const read = name === 'state';
    const answer = await fetch(base + (read ? `/zones/${encodeURIComponent(args.params.room)}/state` : '/volume'), {
      method: read ? 'GET' : 'POST', headers: { 'Content-Type': 'application/json' }, ...(read ? {} : { body: JSON.stringify(args.body) }) });
    return answer.json();
  } });
  assert.equal(await h.api.apply(h.api.nudge(-5)), true); assert.equal(h.api.view().value, 15);
  assert.deepEqual(requests.filter(([method]) => method === 'POST'), [['POST', '/volume', { zone: 'Den@S2', group: true, delta: -5 }]]);
  assert.deepEqual(requests.map(([, url]) => url), ['/zones/Den%40S2/state', '/volume', '/zones/Den%40S2/state']);
});
