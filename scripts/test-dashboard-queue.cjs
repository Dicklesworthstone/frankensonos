'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const zone = { household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen'] };
const item = (z = zone, fresh = true) => ({ zone: z, fresh });
const page = (offset = 0, total = 85) => ({ zone: 'Studio', total, revision: 'opaque-revision', playing: 2,
  items: Array.from({ length: Math.min(40, Math.max(0, total - offset)) }, (_, n) => ({ position: offset + n + 1, title: `Track ${offset + n + 1}`, uri: 'same:duplicate' })) });
const tick = () => new Promise((r) => setImmediate(r));
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
function setup() {
  const calls = []; let handle = (url) => page(Number(new URL(url, 'http://fixture').searchParams.get('offset')));
  const model = Q.model({ call: async (url) => { calls.push(url); return handle(url); } });
  model.observe(item());
  return { model, calls, handle(fn) { handle = fn; }, async ready() { model.open(); await tick(); return model; } };
}
test('native queue is lazy and always addresses the selected household', async () => {
  const f = setup(); assert.equal(f.calls.length, 0); const m = await f.ready();
  assert.equal(f.calls[0], '/zones/Studio%40S2/queue?offset=0&limit=40');
  assert.equal(m.state.page.total, 85); assert.equal(m.state.stale, false);
  assert.equal(m.state.page.items.length, 40);
});
test('duplicate URIs remain distinct positional entries and decoded pages are immutable', () => {
  const p = Q.decode(page(), zone, 0); assert.equal(p.items[0].uri, p.items[1].uri);
  assert.notEqual(p.items[0].position, p.items[1].position); assert.throws(() => p.items.push({}));
});
test('incomplete or wrong-room pages are rejected, never replaced with DJ picks', () => {
  for (const bad of [null, {}, { ...page(), zone: 'Kitchen' }, { ...page(), total: -1 },
    { ...page(), playing: 86 }, { ...page(), items: [] }, { ...page(), revision: '' },
    { ...page(), items: [{ ...page().items[0], position: 2 }, ...page().items.slice(1)] }]) assert.throws(() => Q.decode(bad, zone, 0));
});
test('queue without a revision or active queue playback remains a truthful read-only page', () => {
  const p = Q.decode({ ...page(), playing: null, revision: undefined }, zone, 0);
  assert.equal(p.playing, null); assert.equal(p.revision, null);
});
test('paging replaces the page, never merges revisions or reorders duplicates', async () => {
  const f = setup(), m = await f.ready(); assert.equal(m.navigate(1), true); await tick();
  assert.equal(m.state.page.offset, 40); assert.equal(m.state.page.items[0].position, 41);
  assert.equal(m.navigate(1), true); await tick(); assert.equal(m.state.page.items.length, 5);
  assert.equal(m.navigate(1), false); assert.equal(m.navigate(-1), true); await tick();
  assert.equal(m.state.page.offset, 40);
});
test('a shrunken tail page makes at most one corrective read', async () => {
  const f = setup(), m = await f.ready(); f.handle((url) => page(Number(new URL(url, 'http://fixture').searchParams.get('offset')), 3));
  m.navigate(1); await tick(); assert.equal(f.calls.length, 3); assert.equal(m.state.page.offset, 0); assert.equal(m.state.page.total, 3);
});
test('a failed read preserves the prior page but marks it stale and blocks paging', async () => {
  const f = setup(), m = await f.ready(); f.handle(() => { throw new Error('Offline'); });
  await m.read(); assert.equal(m.state.stale, true); assert.equal(m.state.page.total, 85); assert.equal(m.navigate(1), false);
  assert.match(m.state.error, /Offline/);
});
test('closing during a read fences the old reply and does not start more reads', async () => {
  const f = setup(), d = deferred(); f.handle(() => d.promise); f.model.open(); f.model.close();
  d.resolve(page()); await tick(); assert.equal(f.model.state.page, null); assert.equal(f.calls.length, 1);
});
test('same-name rooms across households cannot accept one another’s late pages', async () => {
  const f = setup(), d = deferred(); f.handle(() => d.promise); f.model.open();
  f.model.observe(item({ ...zone, household: 'S1' })); d.resolve(page()); await tick();
  assert.equal(f.model.state.page, null); f.handle(() => page()); f.model.observe(item({ ...zone, household: 'S1' })); await tick();
  assert.equal(f.calls.at(-1), '/zones/Studio%40S1/queue?offset=0&limit=40');
});
test('hidden queues do not poll, and an explicit resume needs a fresh observation', async () => {
  const f = setup(), m = await f.ready(); m.suspend(); m.invalidate(); m.observe(item()); await tick(); assert.equal(f.calls.length, 1);
  m.resume(); m.observe(item()); await tick(); assert.equal(f.calls.length, 2);
});
test('event storms do not multiply requests, and dirty in-flight pages remain stale', async () => {
  const f = setup(), d = deferred(); f.handle(() => d.promise); f.model.open();
  for (let i = 0; i < 100; i++) { f.model.invalidate(); f.model.observe(item()); }
  assert.equal(f.calls.length, 1); d.resolve(page()); await tick(); assert.equal(f.model.state.stale, true);
  f.handle(() => page()); f.model.observe(item()); await tick(); assert.equal(f.calls.length, 2); assert.equal(f.model.state.stale, false);
});
test('loss of fresh selected state suppresses reads', async () => {
  const f = setup(); f.model.observe(item(zone, false)); f.model.open(); await tick(); assert.equal(f.calls.length, 0);
});
test('disposal is final and issues no additional requests', async () => {
  const f = setup(), m = await f.ready(); m.dispose(); m.open(); m.resume(); m.observe(item()); await m.read();
  assert.equal(f.calls.length, 1);
});
