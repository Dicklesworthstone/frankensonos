// Queue actions execute through the same production mutation lane as the DJ.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const tick = () => new Promise((r) => setImmediate(r));
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
const z = (household = 'S2') => ({ household, coordinator_room: 'Synthetic room', members: ['Synthetic room', 'Second room'], transport_state: 'paused' });
async function fixture({ demo = false, wrap = (call) => call } = {}) {
  const data = { zones: [z(), z('S1')], revision: 'r1', playing: 2, transport: 'paused',
    tracks: Array.from({ length: 55 }, (_, i) => ({ title: `Track ${i + 1}`, uri: i < 5 ? 'same:duplicate' : `synthetic:${i}` })) };
  let hook = null, queue;
  const calls = [];
  const raw = async (url, body) => {
    calls.push({ url, body });
    if (hook) { const value = hook(url, body); if (value !== undefined) return value; }
    if (url === '/zones') return structuredClone(data.zones);
    const target = decodeURIComponent(url.split('/')[2] || '');
    const zone = data.zones.find((zone) => C.key(zone) === target) || data.zones[0];
    if (url.endsWith('/state')) return { zone: structuredClone(zone), transport_state: data.transport,
      volume: 20, track: data.playing ? { uri: data.tracks[data.playing - 1]?.uri || 'synthetic:old', queue_position: data.playing } : null };
    if (url.endsWith('/dj')) return { zone: zone.coordinator_room, running: false, next: [], steering: { source: 'none' } };
    if (url.includes('/queue?')) {
      const offset = Number(new URL(url, 'http://fixture').searchParams.get('offset'));
      return { zone: zone.coordinator_room, total: data.tracks.length, revision: data.revision, playing: data.playing,
        items: data.tracks.slice(offset, offset + 40).map((row, n) => ({ ...row, position: offset + n + 1 })) };
    }
    if (body !== undefined) {
      if (url === '/queue/play') { data.playing = body.position; data.transport = 'playing'; return { done: 'Played selected entry', changed: true }; }
      const count = body.expected_uris?.length, original = data.tracks[data.playing - 1];
      if (url === '/queue/remove') {
        data.tracks.splice(body.position - 1, count); data.playing = Math.min(data.playing, data.tracks.length) || null;
        data.revision += 'x';
        return { done: 'Removed selected entries', zone: zone.coordinator_room, position: body.position, count, remaining: data.tracks.length, recovered: false, undoable: false };
      }
      if (url === '/queue/move') {
        const moved = data.tracks.splice(body.position - 1, count);
        const position = body.insert_before < body.position ? body.insert_before : body.insert_before - count;
        data.tracks.splice(position - 1, 0, ...moved); data.playing = data.tracks.indexOf(original) + 1;
        data.revision += 'x';
        return { done: 'Moved selected entries', zone: zone.coordinator_room, position, count, total: data.tracks.length,
          revision: data.revision, changed: true, recovered: false, undoable: false };
      }
      return { done: 'Transport accepted', changed: true };
    }
    throw new Error(`Unexpected request ${url}`);
  };
  const call = wrap(raw);
  const controller = C.model({ call, demo, now: () => 1000, changed: () => queue?.observe(controller.selected()) });
  const session = S.model({ call, controller, demo, now: () => 1000 });
  queue = Q.model({ call });
  await controller.refresh(); queue.open(); await tick();
  if (queue.state.loading) await new Promise((r) => setTimeout(r, 20));
  return { data, calls, controller, session, queue, call, raw, hook(fn) { hook = fn; },
    writes: () => calls.filter((c) => c.body !== undefined),
    review(action = 'remove', first = 2, count = 1, before) { return session.reviewQueue(queue.select(first, count), action, before); } };
}

for (const action of ['play', 'remove', 'move']) {
  test(`${action} reviews existing queue positions without writing, then dispatches exactly one native command`, async () => {
    const f = await fixture(), r = f.review(action, 3, 1, 8);
    assert.ok(r); assert.equal(r.kind, 'queue'); assert.equal(f.writes().length, 0);
    assert.equal(r.target, 'Synthetic room@S2'); assert.match(r.detail, /#3: Track 3/);
    assert.equal(await f.session.confirm(r), true);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, `/queue/${action}`);
    assert.equal(f.writes()[0].body.expected_revision, 'r1'); assert.equal(f.writes()[0].body.position, 3);
    assert.equal(f.data.transport, action === 'play' ? 'playing' : 'paused');
    assert.equal(f.queue.state.selection, null);
    assert.equal(await f.session.confirm(r), false); assert.equal(f.writes().length, 1);
  });
}

test('consecutive range moves preserve order and use the original destination coordinates', async () => {
  const f = await fixture(), titles = f.data.tracks.slice(2, 5).map((r) => r.title), r = f.review('move', 3, 3, 10);
  assert.equal(await f.session.confirm(r), true);
  assert.deepEqual(f.data.tracks.slice(6, 9).map((r) => r.title), titles);
  assert.deepEqual(f.writes()[0].body.expected_uris, Array(3).fill('same:duplicate'));
  assert.equal(f.data.tracks.length, 55); assert.equal(f.data.transport, 'paused');
});

test('multi-entry removal explains conditional undo and never turns into queue clear', async () => {
  const f = await fixture(), r = f.review('remove', 2, 3);
  assert.match(r.detail, /check the result/); assert.match(r.detail, /currently playing/);
  assert.equal(await f.session.confirm(r), true); assert.equal(f.data.tracks.length, 52);
  assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].body.expected_uris.length, 3);
});

test('a range on a later page can move to the front without changing its absolute source positions', async () => {
  const f = await fixture(); await f.queue.read(40); const r = f.review('move', 42, 3, 1);
  assert.equal(await f.session.confirm(r), true); assert.equal(f.writes()[0].body.position, 42);
  assert.equal(f.data.tracks[0].title, 'Track 42');
});

for (const change of ['revision', 'uri', 'title', 'total']) {
  test(`changed ${change} during review prevents every queue write`, async () => {
    const f = await fixture(), r = f.review('remove', 3);
    if (change === 'revision') f.data.revision = 'r2';
    if (change === 'uri') f.data.tracks[2].uri = 'another:song';
    if (change === 'title') f.data.tracks[2].title = 'Renamed';
    if (change === 'total') f.data.tracks.push({ uri: 'new:song', title: 'New' });
    assert.equal(await f.session.confirm(r), false); assert.equal(f.writes().length, 0);
    assert.match(f.controller.state.message, /queue or selected entries changed/);
  });
}

for (const change of ['membership', 'transport', 'slot']) {
  test(`changed ${change} fails the playback/group preflight before editing the queue`, async () => {
    const f = await fixture(), r = f.review();
    if (change === 'membership') f.data.zones[0].members = ['Synthetic room'];
    if (change === 'transport') f.data.transport = 'playing';
    if (change === 'slot') f.data.playing = 3;
    assert.equal(await f.session.confirm(r), false); assert.equal(f.writes().length, 0);
  });
}

test('same-name groups in another household cannot inherit a retained selection or review', async () => {
  const f = await fixture(), chosen = f.queue.select(2), r = f.session.reviewQueue(chosen, 'remove');
  f.controller.select('Synthetic room@S1');
  assert.equal(await f.session.confirm(r), false); f.session.cancel();
  assert.equal(f.session.reviewQueue(chosen, 'play'), null); assert.equal(f.writes().length, 0);
});

test('an active review is not overwritten by another queue action', async () => {
  const f = await fixture(), r = f.review();
  assert.equal(f.session.reviewQueue(f.queue.state.selection, 'play'), null); assert.equal(f.session.state.pending, r);
});

test('refreshing the queue consumes its earlier review even when the revision did not change', async () => {
  const f = await fixture(), r = f.review(); await f.queue.read();
  assert.equal(await f.session.confirm(r), false); assert.equal(f.writes().length, 0);
});

for (const boundary of ['queue-close', 'queue-suspend', 'cancel', 'hide', 'connection-loss']) {
  test(`${boundary} during queue preflight prevents dispatch`, async () => {
    const f = await fixture(), r = f.review(), d = deferred();
    let observed; f.hook((url) => {
      if (url.includes('/queue?')) { observed = url; return d.promise; }
    });
    const pending = f.session.confirm(r); await tick(); assert.ok(observed);
    if (boundary === 'queue-close') f.queue.close();
    if (boundary === 'queue-suspend') f.queue.suspend();
    if (boundary === 'cancel') f.session.cancel();
    if (boundary === 'hide') { f.controller.suspend(); f.session.suspend(); f.queue.suspend(); }
    if (boundary === 'connection-loss') f.controller.invalidate();
    f.hook(null); d.resolve(await f.raw(observed));
    assert.equal(await pending, false); assert.equal(f.writes().length, 0);
  });
}

test('queue controls share transport exclusion and consume the submitted selection immediately', async () => {
  const f = await fixture(), r = f.review(), d = deferred();
  f.hook((url, body) => body !== undefined ? d.promise : undefined);
  const pending = f.session.confirm(r); await tick();
  assert.equal(f.controller.state.busy, true); assert.equal(f.queue.state.selection, null);
  assert.equal(await f.controller.command('next'), false); assert.equal(await f.session.confirm(r), false);
  assert.equal(f.controller.select('Synthetic room@S1'), false); assert.equal(f.writes().length, 1);
  d.resolve({ done: 'Removed', zone: 'Synthetic room', position: 2, count: 1, remaining: 54, recovered: false, undoable: false });
  assert.equal(await pending, true);
});

test('a submitted command can finish while hidden without resending or inventing uncertainty', async () => {
  const f = await fixture(), r = f.review('play'), d = deferred();
  f.hook((url, body) => body !== undefined ? d.promise : undefined);
  const pending = f.session.confirm(r); await tick();
  f.controller.suspend(); f.session.suspend(); f.queue.suspend();
  d.resolve({ done: 'Played while hidden', changed: true });
  assert.equal(await pending, true); assert.equal(f.controller.state.uncertain, false);
  assert.equal(f.controller.canControl(), false); assert.match(f.controller.state.message, /Played while hidden/); assert.equal(f.writes().length, 1);
});

for (const response of [null, {}, { done: 'Removed', changed: true }, { done: 'Removed', zone: 'Other', position: 2, count: 1, remaining: 54, recovered: false, undoable: false }]) {
  test(`malformed removal acknowledgement ${JSON.stringify(response)} stays uncertain`, async () => {
    const f = await fixture(), r = f.review(); f.hook((url, body) => body !== undefined ? Promise.resolve(response) : undefined);
    assert.equal(await f.session.confirm(r), false); assert.equal(f.controller.state.uncertain, true);
    f.hook(null); await f.controller.refresh(); await tick();
    assert.equal(f.review(), null); assert.equal(f.writes().length, 1);
    await f.controller.refresh(true); await tick(); assert.ok(f.review()); assert.equal(f.writes().length, 1);
  });
}

test('a failed write keeps the API remedy and sends no automatic retry', async () => {
  const f = await fixture(), r = f.review();
  f.hook((url, body) => { if (body !== undefined) throw Object.assign(new Error('Policy denied'), { hint: 'Ask the owner.' }); });
  assert.equal(await f.session.confirm(r), false); assert.match(f.controller.state.message, /Ask the owner/); assert.equal(f.writes().length, 1);
});

test('read-back-confirmed native edits are reported as recovery, not a second write', async () => {
  const f = await fixture(), r = f.review('move', 2, 2, 7);
  f.hook((url, body) => body !== undefined ? { done: 'Move confirmed', zone: 'Synthetic room', position: 5, count: 2, total: 55,
    revision: 'r2', changed: true, recovered: true, undoable: false } : undefined);
  assert.equal(await f.session.confirm(r), true); assert.match(f.controller.state.message, /edit was not replayed/); assert.equal(f.writes().length, 1);
});

test('preview can construct a review but never dispatch a queue command', async () => {
  const f = await fixture({ demo: true }), r = f.review('play'); assert.ok(r);
  assert.equal(await f.session.confirm(r), false); assert.equal(f.writes().length, 0);
});

test('missing renderer identities or revisions cannot produce a reviewed command', async () => {
  const f = await fixture(); f.data.revision = null; await f.queue.read();
  assert.equal(f.review(), null); assert.equal(f.writes().length, 0);
});

for (const behavior of ['play', 'remove', 'move', 'lost', 'malformed', 'denied']) {
  test(`real HTTP ${behavior}: native payloads and uncertain outcomes remain single-dispatch`, async (t) => {
    let backend, observed = [];
    const server = http.createServer(async (req, res) => {
      let raw = ''; for await (const part of req) raw += part;
      const body = raw ? JSON.parse(raw) : undefined; observed.push({ method: req.method, url: req.url, body });
      if (body !== undefined && behavior === 'denied') {
        res.writeHead(403, { 'content-type': 'application/json' }); res.end(JSON.stringify({ detail: 'Denied', hint: 'Ask the owner.' })); return;
      }
      const result = await backend(req.url, body);
      if (body !== undefined && behavior === 'lost') { req.socket.destroy(); return; }
      res.setHeader('content-type', 'application/json'); res.end(body !== undefined && behavior === 'malformed' ? '{' : JSON.stringify(result));
    });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    t.after(() => new Promise((r) => { server.closeAllConnections(); server.close(r); }));
    const base = `http://127.0.0.1:${server.address().port}`;
    const f = await fixture({ wrap(raw) { backend = raw; return C.request({ fetcher: (url, init) => fetch(base + url, init) }); } });
    while (f.queue.state.loading) await tick();
    const action = ['play', 'remove', 'move'].includes(behavior) ? behavior : 'remove';
    const r = f.review(action, 3, 1, 8); assert.ok(r);
    assert.equal(await f.session.confirm(r), ['play', 'remove', 'move'].includes(behavior));
    assert.deepEqual(observed.filter((c) => c.method === 'POST'), [{ method: 'POST', url: `/queue/${action}`, body: r.body }]);
    if (['lost', 'malformed', 'denied'].includes(behavior)) {
      assert.equal(f.controller.state.uncertain, true); await f.controller.refresh(); await tick();
      assert.equal(f.controller.canControl(), false);
      await f.controller.refresh(true); assert.equal(f.controller.canControl(), true);
      assert.equal(observed.filter((c) => c.method === 'POST').length, 1);
    }
  });
}
