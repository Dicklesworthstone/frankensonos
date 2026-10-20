// Run with node --test scripts/test-remote-queue-navigation.cjs.
// These tests execute the shipped queue model; the fixture only supplies I/O.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const Queue = vm.runInNewContext(fs.readFileSync(path.join(__dirname,
  '../crates/fsonos-api/src/remote/queue.js'), 'utf8') + '\nQueueRemote;');
const plain = value => JSON.parse(JSON.stringify(value));
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture(options = {}) {
  const room = options.room || 'Room@S2';
  const calls = [];
  const source = { total: 180, revision: 'q:observed', playing: 26, ...options.source };
  const page = (offset, limit) => ({ ...source,
    items: Array.from({ length: Math.min(limit, Math.max(0, source.total - offset)) }, (_, i) => ({
      position: offset + i + 1, title: `Entry ${offset + i + 1}`, uri: `renderer:${offset + i + 1}`,
    })),
  });
  const call = async (name, args) => {
    calls.push(plain([name, args]));
    if (name === 'queue') {
      const answer = page(args.query.offset, args.query.limit);
      return options.read ? options.read(answer, args, calls.length) : answer;
    }
    const body = args.body;
    assert.equal(body.expected_revision, source.revision);
    assert.equal(body.zone, room);
    assert.deepEqual(plain(body.expected_uris),
      page(body.position - 1, body.expected_uris.length).items.map(item => item.uri));
    source.revision += ':next';
    if (name === 'queueMove') return { done: 'Moved once', zone: room,
      position: body.insert_before < body.position ? body.insert_before : body.insert_before - body.expected_uris.length,
      count: body.expected_uris.length, total: source.total, revision: source.revision,
      changed: true, recovered: false, undoable: false };
    assert.equal(name, 'queueRemove', 'no alternate mutation');
    source.total -= body.expected_uris.length;
    return { done: 'Removed once', position: body.position, count: body.expected_uris.length,
      remaining: source.total, recovered: false, undoable: false };
  };
  const c = Queue.model({ zone: room, call, current: options.current,
    blocked: options.blocked, execute: options.execute });
  return { c, calls, page, source, call };
}

test('checkboxes on adjacent pages select one contiguous block for a single native edit', async () => {
  for (const household of ['S1', 'S2']) {
    const { c, calls } = fixture({ room: `Room@${household}` });
    await c.load(); c.select(24, true); c.select(25, true);
    assert.equal(await c.browse(25), true);
    c.select(26, true); c.select(27, true);
    assert.deepEqual(plain(c.removable().map(item => item.position)), [24, 25, 26, 27]);
    c.reviewMove(1);
    assert.equal(await c.move(), true);
    assert.deepEqual(calls.filter(([name]) => name !== 'queue'), [['queueMove', { body: {
      zone: `Room@${household}`, position: 24, insert_before: 1,
      expected_uris: ['renderer:24', 'renderer:25', 'renderer:26', 'renderer:27'], expected_revision: 'q:observed',
    } }]]);
  }
});

test('a range remains selected while visiting its middle, end and unrelated pages', async () => {
  const { c, calls } = fixture();
  await c.load(); await c.selectRange(24, 123);
  for (const position of [61, 180, 24, 1]) {
    assert.equal(await c.jumpTo(position), true);
    assert.equal(c.state.selected.size, 100);
    assert.equal(c.removable().length, 100);
    assert.equal(c.state.offset, Math.floor((position - 1) / 25) * 25);
    assert.ok(c.state.page.items.some(item => item.position === position));
  }
  assert.ok(calls.every(([name]) => name === 'queue'));
  assert.equal(c.select(1, true), false, 'a new page cannot bypass the 100-entry cap');
});

test('every navigation consumes consent even when the destination is the same page', async () => {
  for (const navigate of [c => c.browse(0), c => c.jumpTo(24), c => c.showCurrent()]) {
    const { c, calls } = fixture();
    await c.load(); c.select(24, true); c.reviewRemoval();
    const old = c.state.pending;
    assert.equal(await navigate(c), true);
    assert.equal(c.state.pending, null);
    assert.equal(c.state.selected.size, 1);
    assert.equal(await c.remove(old), false);
    assert.ok(calls.every(([name]) => name === 'queue'));
    c.reviewRemoval(); assert.notEqual(c.state.pending, old);
    assert.equal(await c.remove(c.state.pending), true);
  }
});

test('explicit refresh still clears a selection even when the queue revision matches', async () => {
  const { c } = fixture();
  await c.load(); await c.selectRange(24, 80);
  await c.browse(25); assert.equal(c.state.selected.size, 57);
  await c.load(); assert.equal(c.state.selected.size, 0);
  assert.equal(c.removable(), null);
});

test('changed revision, length or missing revision clears the previous selection while browsing', async () => {
  for (const change of [s => { s.revision = 'q:new'; }, s => { s.total = 170; }, s => { s.revision = null; }]) {
    const { c, source } = fixture();
    await c.load(); c.select(24, true); change(source);
    assert.equal(await c.browse(25), true);
    assert.equal(c.state.selected.size, 0);
    assert.match(c.state.message, /selection was cleared/);
    assert.equal(c.reviewRemoval(), false);
  }
});

test('a same-revision identity contradiction refuses the destination page and clears selection', async () => {
  const { c } = fixture({ read: (p, args, n) => n === 3
    ? { ...p, items: p.items.map(item => item.position === 26 ? { ...item, uri: 'renderer:other' } : item) } : p });
  await c.load(); await c.selectRange(24, 28);
  const original = c.state.page;
  assert.equal(await c.browse(25), false);
  assert.equal(c.state.page, original, 'old page is retained visibly stale');
  assert.equal(c.state.offset, 0);
  assert.equal(c.state.stale, true); assert.equal(c.state.selected.size, 0);
  assert.match(c.state.message, /disagree/);
});

test('shrinking a queue while navigating corrects the page once without retaining the old block', async () => {
  const { c, calls, source } = fixture();
  await c.load(); c.select(24, true);
  source.total = 26; source.revision = 'q:shorter';
  assert.equal(await c.browse(175), true);
  assert.equal(c.state.offset, 25); assert.equal(c.state.selected.size, 0);
  assert.deepEqual(calls.map(([, args]) => args.query.offset), [0, 175, 25]);
});

test('legacy pages can be browsed without a queue revision but cannot become editable', async () => {
  const { c, calls } = fixture({ source: { revision: null } });
  await c.load(); assert.equal(await c.jumpTo(60), true);
  assert.equal(c.state.offset, 50); assert.equal(c.select(51, true), false);
  assert.equal(c.reviewMove(1), false); assert.equal(c.reviewRemoval(), false);
  assert.ok(calls.every(([name]) => name === 'queue'));
});

test('invalid positions and offsets make no reads and cannot become commands', async () => {
  const { c, calls } = fixture(); await c.load();
  for (const position of [0, -1, 181, 1.5, NaN, Infinity, '26', null, undefined, 4294967296]) {
    assert.equal(await c.jumpTo(position), false);
  }
  for (const offset of [-1, '25', 0.5, NaN, Infinity, 4294967295]) assert.equal(await c.browse(offset), false);
  assert.equal(calls.length, 1);
});

test('large queue positions remain unsigned and the last read does not overflow the API range', async () => {
  const { c, calls } = fixture({ source: { total: 4294967295, playing: 4294967295 } });
  await c.load(); assert.equal(await c.jumpTo(4294967295), true);
  assert.equal(c.state.offset, 4294967270);
  assert.equal(c.state.page.items.at(-1).position, 4294967295);
  assert.equal(calls.at(-1)[1].query.offset + calls.at(-1)[1].query.limit, 4294967295);
});

for (const phase of ['page', 'locate', 'destination']) {
  test(`one ${phase} request stays occupied across invalidation until its underlying read settles`, async () => {
    for (const action of ['stale', 'close', 'suspend', 'dispose', 'block', 'replace']) {
      const wait = deferred(); let current = true, blocked = false;
      const heldAt = phase === 'destination' ? 3 : 2;
      const { c, calls, page } = fixture({ current: () => current, blocked: () => blocked,
        read: (p, args, n) => n === heldAt ? wait.promise : p });
      await c.load(); c.select(24, true); c.reviewMove(1);
      const work = phase === 'page' ? c.browse(25) : c.showCurrent();
      while (calls.length < heldAt) await Promise.resolve();
      assert.equal(c.state.navigating, true);
      if (action === 'replace') current = false;
      else if (action === 'block') { blocked = true; c.block(); blocked = false; }
      else c[action]();
      for (let i = 0; i < 10; i++) {
        assert.equal(await c.browse(25), false); assert.equal(await c.load(), false);
        assert.equal(await c.showCurrent(), false); assert.equal(await c.reconcile(), false);
        assert.equal(await c.selectRange(24, 28), false); assert.equal(await c.play(1), false);
      }
      wait.resolve(page(phase === 'locate' ? 0 : 25, phase === 'locate' ? 1 : 25));
      assert.equal(await work, false); assert.equal(c.state.navigating, false);
      assert.equal(c.state.pending, null); assert.equal(calls.length, heldAt);
      assert.ok(calls.every(([name]) => name === 'queue'));
    }
  });
}

test('failed browsing preserves the old page and remedy, not a usable selection or an automatic retry', async () => {
  const { c, calls } = fixture({ read: (p, args, n) => {
    if (n === 2) throw Object.assign(new Error('Read failed'), { hint: 'Refresh after discovery.' });
    return p;
  } });
  await c.load(); c.select(24, true); const old = c.state.page;
  assert.equal(await c.browse(25), false);
  assert.equal(c.state.page, old); assert.equal(c.state.stale, true); assert.equal(c.state.selected.size, 0);
  assert.match(c.state.message, /Read failed.*Refresh after discovery/);
  assert.equal(calls.length, 2);
  assert.equal(await c.browse(25), false);
  await c.load(); assert.equal(calls.length, 3);
});

test('show current reads a fresh pointer and then one page, never using the old displayed pointer', async () => {
  const { c, calls, source } = fixture();
  await c.load(); c.select(24, true); source.playing = 151;
  assert.equal(await c.showCurrent(), true);
  assert.equal(c.state.offset, 150); assert.equal(c.state.page.playing, 151);
  assert.equal(c.state.selected.size, 1);
  assert.deepEqual(calls.map(([, args]) => args.query), [
    { offset: 0, limit: 25 }, { offset: 0, limit: 1 }, { offset: 150, limit: 25 },
  ]);
});

test('show current clears old identities when its new anchor reports a changed queue', async () => {
  const { c, source } = fixture();
  await c.load(); c.select(24, true); source.revision = 'q:new'; source.playing = 100;
  assert.equal(await c.showCurrent(), true);
  assert.equal(c.state.offset, 75); assert.equal(c.state.page.revision, 'q:new');
  assert.equal(c.state.selected.size, 0);
});

test('queue changes between the current-pointer read and destination page fail without chasing', async () => {
  const { c, calls } = fixture({ read: (p, args, n) => n === 3 ? { ...p, revision: 'q:changed' } : p });
  await c.load(); c.select(24, true);
  assert.equal(await c.showCurrent(), false);
  assert.equal(c.state.stale, true); assert.equal(c.state.selected.size, 0);
  assert.match(c.state.message, /changed while locating/); assert.equal(calls.length, 3);
});

test('an advancing pointer outside the page is reported instead of starting an unbounded follow loop', async () => {
  const { c, calls } = fixture({ read: (p, args, n) => n === 3 ? { ...p, playing: 155 } : p });
  await c.load(); assert.equal(await c.showCurrent(), true);
  assert.equal(c.state.offset, 25); assert.equal(c.state.page.playing, 155);
  assert.match(c.state.message, /advanced.*again/); assert.equal(calls.length, 3);
});

test('no current queue entry stops after the pointer read without inferring play, seek or resume', async () => {
  for (const change of [false, true]) {
    const { c, source, calls } = fixture();
    await c.load(); c.select(24, true);
    source.playing = null; if (change) source.revision = 'q:new';
    assert.equal(await c.showCurrent(), false);
    assert.match(c.state.message, /No current queue entry/);
    assert.equal(c.state.selected.size, change ? 0 : 1);
    assert.equal(c.state.stale, change);
    if (!change) assert.equal(c.state.page.playing, null);
    assert.equal(calls.length, 2);
  }
});

test('bad pointer responses cannot launch a destination read', async () => {
  for (const change of [p => null, p => ({ ...p, items: [] }),
    p => ({ ...p, items: [...p.items, { position: 2, title: 'Extra', uri: 'extra' }] })]) {
    const { c, calls } = fixture({ read: (p, args, n) => n === 2 ? change(p) : p });
    await c.load(); assert.equal(await c.showCurrent(), false);
    assert.equal(calls.length, 2); assert.equal(c.state.stale, true);
  }
});

test('blocked, closed, uncertain and pending-edit queues cannot start navigation reads', async () => {
  let blocked = false;
  const { c, calls } = fixture({ blocked: () => blocked });
  await c.load(); blocked = true;
  assert.equal(await c.browse(25), false); assert.equal(await c.showCurrent(), false);
  blocked = false; c.close(); assert.equal(await c.jumpTo(30), false);
  await c.load(); c.state.uncertain = true;
  assert.equal(await c.browse(25), false); assert.equal(await c.showCurrent(), false);
  c.state.uncertain = false; c.state.writing = true;
  assert.equal(await c.showCurrent(), false); assert.equal(await c.jumpTo(30), false);
  assert.equal(calls.length, 2);
});

test('real localhost paging retains selection and sends one original-revision removal', async t => {
  const room = 'Room + / @S1'; const f = fixture({ room }); const observed = [];
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://localhost'); let answer;
      if (req.method === 'GET') {
        assert.equal(decodeURIComponent(url.pathname), `/zones/${room}/queue`);
        const query = { offset: Number(url.searchParams.get('offset')), limit: Number(url.searchParams.get('limit')) };
        observed.push(['GET', query]); answer = await f.call('queue', { params: { room }, query });
      } else {
        assert.equal(req.method, 'POST'); assert.equal(url.pathname, '/queue/remove');
        let text = ''; for await (const chunk of req) text += chunk;
        const body = JSON.parse(text); observed.push(['POST', body]);
        answer = await f.call('queueRemove', { body });
      }
      res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(answer));
    } catch (error) { res.writeHead(500); res.end(JSON.stringify({ detail: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const c = Queue.model({ zone: room, call: async (name, args) => {
    const read = name === 'queue';
    const url = read ? `${origin}/zones/${encodeURIComponent(args.params.room)}/queue?${new URLSearchParams(args.query)}` : `${origin}/queue/remove`;
    const response = await fetch(url, read ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(args.body) });
    assert.equal(response.status, 200); return response.json();
  } });
  await c.load(); c.select(25, true); await c.browse(25); c.select(26, true);
  await c.jumpTo(180); c.reviewRemoval(); assert.equal(await c.remove(), true);
  assert.deepEqual(observed.map(([method]) => method), ['GET', 'GET', 'GET', 'POST', 'GET']);
  assert.deepEqual(observed[3][1], { zone: room, position: 25, expected_uris: ['renderer:25', 'renderer:26'], expected_revision: 'q:observed' });
});
