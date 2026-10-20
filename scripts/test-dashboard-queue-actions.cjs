// Native queue contracts and selection lifetimes; no speakers or account needed.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const zone = { household: 'S2', coordinator_room: 'Synthetic / room', members: ['Synthetic / room', 'Other'] };
const tick = () => new Promise((r) => setImmediate(r));
const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { resolve, promise }; };
function page(offset = 0, total = 85) {
  return { zone: zone.coordinator_room, total, playing: 2, revision: 'synthetic:revision',
    items: Array.from({ length: Math.min(40, Math.max(0, total - offset)) }, (_, n) => ({
      position: offset + n + 1, title: `Track ${offset + n + 1}`, uri: n < 4 ? 'same:duplicate' : `song:${offset + n}`,
    })) };
}
async function fixture(edit = (v) => v) {
  const calls = []; let respond = (url) => edit(page(Number(new URL(url, 'http://fixture').searchParams.get('offset'))));
  const item = { zone: structuredClone(zone), fresh: true };
  const call = async (url, body) => { calls.push({ url, body }); return respond(url); };
  const model = Q.model({ call }); model.observe(item); model.open(); await tick();
  return { model, calls, item, call, respond(fn) { respond = fn; } };
}
function removed(op, extra = {}) {
  return { done: 'Removed', zone: zone.coordinator_room, position: op.body.position, count: op.body.expected_uris.length,
    remaining: op.choice.page.total - op.body.expected_uris.length, recovered: false, undoable: false, ...extra };
}
function moved(op, extra = {}) {
  return { done: 'Moved', zone: zone.coordinator_room, position: op.resultPosition, count: op.body.expected_uris.length,
    total: op.choice.page.total, revision: 'new:revision', changed: true, recovered: false, undoable: false, ...extra };
}

test('selection and review are read-only, immutable and preserve repeated URI positions', async () => {
  const f = await fixture(), choice = f.model.select(2, 3);
  assert.equal(Q.choiceOf(choice), choice); assert.equal(f.model.state.selection, choice);
  assert.deepEqual(choice.rows.map((r) => r.position), [2, 3, 4]);
  assert.deepEqual(choice.rows.map((r) => r.uri), Array(3).fill('same:duplicate'));
  assert.throws(() => choice.rows.push({})); assert.throws(() => { choice.zone.members[0] = 'Changed'; });
  const op = Q.plan(choice, 'remove');
  assert.deepEqual(op.body, { zone: 'Synthetic / room@S2', position: 2, expected_revision: 'synthetic:revision', expected_uris: Array(3).fill('same:duplicate') });
  assert.match(op.detail, /Not undoable/); assert.match(op.detail, /currently playing/);
  assert.equal(f.calls.length, 1); assert.ok(f.calls.every((c) => c.body === undefined));
});

test('play pins one existing renderer URI and revision, not a Spotify reconstruction', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(3), 'play');
  assert.equal(op.url, '/queue/play');
  assert.deepEqual(op.body, { zone: 'Synthetic / room@S2', position: 3, expected_revision: 'synthetic:revision', expected_uri: 'same:duplicate' });
  assert.match(op.detail, /including when paused/); assert.throws(() => Q.plan(f.model.select(3, 2), 'play'));
});

for (const [from, count, before, after] of [[3, 2, 1, 1], [3, 2, 6, 4], [3, 2, 86, 84], [38, 3, 2, 2], [1, 1, 3, 2]]) {
  test(`move original #${from} count ${count} before #${before} lands at #${after}`, async () => {
    const f = await fixture(), op = Q.plan(f.model.select(from, count), 'move', before);
    assert.equal(op.body.insert_before, before); assert.equal(op.resultPosition, after);
    assert.equal(op.body.expected_uris.length, count); assert.match(op.detail, /Does not restart or resume/);
    assert.equal(Q.receipt(moved(op), op).changed, true);
  });
}

test('page-two selections keep absolute positions and can move before any original slot', async () => {
  const f = await fixture(); await f.model.read(40);
  const op = Q.plan(f.model.select(42, 3), 'move', 5);
  assert.equal(op.body.position, 42); assert.equal(op.choice.page.offset, 40); assert.equal(op.resultPosition, 5);
  await Q.verify(f.call, op); assert.match(f.calls.at(-1).url, /offset=40&limit=40$/);
});

test('invalid ranges, missing identities and unavailable revisions remain read-only', async () => {
  const f = await fixture();
  for (const [first, count] of [[0, 1], [1, 0], [1, -1], [1, 1.5], [1, 41], [40, 2], [41, 1], [NaN, 1], [2.5, 1]]) {
    assert.equal(f.model.select(first, count), null);
  }
  for (const revision of [undefined, ' ', 'x'.repeat(1025), '😀'.repeat(257)]) {
    const g = await fixture((p) => ({ ...p, revision })); assert.equal(g.model.select(1), null);
  }
  for (const uri of [null, '', ' ', 'NOT_IMPLEMENTED']) {
    const g = await fixture((p) => { p.items[1].uri = uri; return p; }); assert.equal(g.model.select(1, 3), null);
  }
});

test('out-of-bounds and no-op move destinations never create a command', async () => {
  const f = await fixture(), choice = f.model.select(3, 2);
  for (const dest of [0, -1, 3, 4, 5, 87, 3.5, '6', NaN, Infinity]) assert.throws(() => Q.plan(choice, 'move', dest));
  assert.throws(() => Q.plan(choice, 'clear')); assert.equal(f.calls.length, 1);
});

test('last representable queue position cannot overflow the native destination', async () => {
  const f = await fixture((p) => ({ ...p, total: 0xffffffff }));
  assert.throws(() => Q.plan(f.model.select(2), 'move', 0x100000000));
});

test('an arbitrary or cloned object cannot impersonate a live queue selection', async () => {
  const f = await fixture(), choice = f.model.select(2);
  for (const value of [null, {}, structuredClone(choice), { ...choice }, 3, 'selection']) assert.throws(() => Q.choiceOf(value));
});

for (const action of ['close', 'suspend', 'dispose', 'invalidate', 'clearSelection']) {
  test(`${action} retires selection authority without sending anything`, async () => {
    const f = await fixture(), choice = f.model.select(2);
    f.model[action](); assert.throws(() => Q.plan(choice, 'remove'));
    assert.ok(f.calls.every((c) => c.body === undefined));
  });
}

test('a new selection consumes the old selection, including at the same position', async () => {
  const f = await fixture(), first = f.model.select(2), second = f.model.select(2);
  assert.throws(() => Q.choiceOf(first)); assert.equal(Q.choiceOf(second), second);
});

test('retained page controls cannot select an entry after refresh or navigation', async () => {
  const f = await fixture(), old = f.model.state.page, chosen = f.model.select(2);
  await f.model.read(); assert.equal(f.model.select(2, 1, old), null); assert.throws(() => Q.choiceOf(chosen));
});

test('an in-flight refresh blocks selection, even before any replacement reply', async () => {
  const f = await fixture(), d = deferred(), choice = f.model.select(2);
  f.respond(() => d.promise); const reading = f.model.read();
  assert.equal(f.model.select(2), null); assert.throws(() => Q.choiceOf(choice));
  d.resolve(page()); await reading;
});

test('losing fresh room state retires the choice even if queue contents remain visible', async () => {
  const f = await fixture(), choice = f.model.select(2); f.item.fresh = false; f.model.observe(f.item);
  assert.equal(f.model.state.page.total, 85); assert.equal(f.model.state.selection, null); assert.throws(() => Q.choiceOf(choice));
});

for (const change of ['household', 'coordinator', 'members']) {
  test(`changed ${change} cannot reuse a same-position queue choice`, async () => {
    const f = await fixture(), choice = f.model.select(2), z = structuredClone(zone);
    if (change === 'household') z.household = 'S1';
    if (change === 'coordinator') z.coordinator_room = 'Other';
    if (change === 'members') z.members = [z.coordinator_room];
    f.model.observe({ zone: z, fresh: false }); assert.throws(() => Q.choiceOf(choice)); assert.equal(f.model.state.selection, null);
  });
}

for (const change of ['revision', 'total', 'uri', 'title', 'album', 'creator']) {
  test(`preflight rejects changed ${change} before a queue write`, async () => {
    const f = await fixture(), op = Q.plan(f.model.select(2), 'remove'), next = page();
    if (change === 'revision') next.revision = 'another';
    else if (change === 'total') next.total++;
    else next.items[1][change] = 'changed';
    f.respond(() => next); await assert.rejects(Q.verify(f.call, op), /changed/);
    assert.ok(f.calls.every((c) => c.body === undefined));
  });
}

test('preflight rechecks external command admission and selection lifetime after the read', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2), 'remove'), d = deferred(); let checks = 0;
  f.respond(() => d.promise); const pending = Q.verify(f.call, op, () => { checks++; });
  f.model.close(); d.resolve(page()); await assert.rejects(pending, /no longer current/); assert.equal(checks, 2);
});

test('a throwing preflight guard prevents even the queue read', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2), 'remove');
  await assert.rejects(Q.verify(f.call, op, () => { throw new Error('Cancelled'); }), /Cancelled/); assert.equal(f.calls.length, 1);
});

test('acknowledgements remain usable after the originating view is hidden', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2, 3), 'remove'); f.model.suspend();
  assert.equal(Q.receipt(removed(op, { recovered: true }), op).changed, true);
  assert.match(Q.receipt(removed(op, { recovered: true }), op).notes[0].detail, /not replayed/);
});

test('removal receipts must match the group, selected range, remaining length and undo boundary', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2, 3), 'remove');
  for (const patch of [{ zone: 'Other' }, { position: 1 }, { count: 2 }, { remaining: 83 }, { undoable: true }, { recovered: 'yes' }, { done: '' }, { notes: [{}] }]) {
    assert.throws(() => Q.receipt(removed(op, patch), op));
  }
  assert.equal(Q.receipt(removed(op), op).done, 'Removed');
});

test('move receipts validate the resulting position rather than the original insert-before', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2, 3), 'move', 10);
  assert.equal(op.resultPosition, 7);
  for (const patch of [{ position: 10 }, { count: 1 }, { total: 82 }, { revision: '' }, { changed: 'yes' }, { undoable: true }, { zone: 'Other' }]) {
    assert.throws(() => Q.receipt(moved(op, patch), op));
  }
  assert.equal(Q.receipt(moved(op, { changed: false }), op).changed, false);
});

test('play receipts need a complete native outcome; malformed successes are not acknowledgements', async () => {
  const f = await fixture(), op = Q.plan(f.model.select(2), 'play');
  for (const value of [null, {}, { done: 'Played' }, { done: 'Played', changed: 'yes' }, { done: 'Played', changed: true, notes: [null] }]) assert.throws(() => Q.receipt(value, op));
  assert.equal(Q.receipt({ done: 'Played', changed: true }, op).changed, true);
});

test('real HTTP queue preflight preserves target escaping, page positions and duplicate identities', async (t) => {
  const seen = [];
  const server = http.createServer((req, res) => {
    seen.push([req.method, req.url]); res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(page(Number(new URL(req.url, 'http://fixture').searchParams.get('offset')))));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  t.after(() => new Promise((r) => { server.closeAllConnections(); server.close(r); }));
  const call = async (url) => (await fetch(`http://127.0.0.1:${server.address().port}${url}`)).json();
  const model = Q.model({ call }); model.observe({ zone, fresh: true });
  model.state.open = true; await model.read(40);
  const op = Q.plan(model.select(42, 2), 'move', 1); await Q.verify(call, op);
  assert.deepEqual(seen, Array(2).fill(['GET', '/zones/Synthetic%20%2F%20room%40S2/queue?offset=40&limit=40']));
  assert.equal(op.body.position, 42); assert.equal(op.body.expected_uris.length, 2);
});
