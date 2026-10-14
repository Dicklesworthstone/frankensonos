// Run with node --test scripts/test-remote-queue.cjs (no packages required).
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const QueueRemote = vm.runInNewContext(readFileSync(join(__dirname, '../crates/fsonos-api/src/remote/queue.js'), 'utf8') + '\nQueueRemote;');
const plain = (value) => JSON.parse(JSON.stringify(value));
const page = (offset = 0, total = 60, revision = 'observed revision') => ({
  total, revision, playing: 2,
  items: Array.from({ length: Math.min(25, Math.max(0, total - offset)) }, (_, i) => ({
    position: offset + i + 1, title: `Track ${offset + i + 1}`, uri: `renderer:${offset + i + 1}`,
  })),
});
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// These run the shipped JavaScript, not a parallel model of its behavior.
test('browse bounds pages and forwards room names as parameters', async () => {
  const calls = [];
  const c = QueueRemote.model({ zone: "Room / ' @S2", call: async (name, args) => { calls.push([name, args]); return page(args.query.offset); } });
  await c.load();
  await c.load(25);
  assert.deepEqual(plain(calls), [
    ['queue', { params: { room: "Room / ' @S2" }, query: { offset: 0, limit: 25 } }],
    ['queue', { params: { room: "Room / ' @S2" }, query: { offset: 25, limit: 25 } }],
  ]);
  assert.equal(c.state.page.items[0].position, 26);
});

test('play preserves exact renderer identity and opaque revision', async () => {
  const calls = [];
  const p = page(); p.items[1].uri = 'x-sonos-spotify:track%3AExact?sid=12&sn=2';
  const c = QueueRemote.model({ zone: 'Room@S2', call: async (name, args) => {
    calls.push([name, args]); return name === 'queue' ? p : { done: 'Playing', changed: true, notes: [{ detail: 'Volume capped' }] };
  } });
  await c.load();
  assert.equal(await c.play(2), true);
  assert.deepEqual(plain(calls[1]), ['queuePlay', { body: {
    zone: 'Room@S2', position: 2, expected_uri: p.items[1].uri, expected_revision: p.revision,
  } }]);
  assert.equal(calls.length, 3, 'one write and a read refresh');
  assert.match(c.state.message, /Volume capped/);
});

test('a double tap cannot replay playback while its reply is pending', async () => {
  const waiting = deferred(); let writes = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async (name) => {
    if (name === 'queue') return page();
    writes++; return waiting.promise;
  } });
  await c.load();
  const first = c.play(1);
  assert.equal(await c.play(1), false);
  assert.equal(await c.load(), false);
  waiting.resolve({ done: 'Playing', changed: true });
  await first;
  assert.equal(writes, 1);
});

test('failure or unreadable success invalidates selection and never retries', async () => {
  for (const response of [null, {}, { done: 'maybe' }, new Error('reply lost')]) {
    const calls = [];
    const c = QueueRemote.model({ zone: 'Room', call: async (name) => {
      calls.push(name);
      if (name === 'queue') return page();
      if (response instanceof Error) throw response;
      return response;
    } });
    await c.load();
    assert.equal(await c.play(1), false);
    assert.equal(await c.play(1), false);
    assert.equal(c.state.page, null);
    assert.deepEqual(calls, ['queue', 'queuePlay']);
    assert.match(c.state.message, /not repeated/);
  }
});

test('old reads cannot overwrite newer pages', async () => {
  const first = deferred(), second = deferred(); let reads = 0;
  const c = QueueRemote.model({ zone: 'Room', call: () => (++reads === 1 ? first : second).promise });
  const a = c.load(0), b = c.load(25);
  second.resolve(page(25)); await b;
  first.resolve(page(0)); await a;
  assert.equal(c.state.offset, 25);
  assert.equal(c.state.page.items[0].position, 26);
});

test('disposed cards ignore late reads and cannot send actions', async () => {
  const read = deferred(); let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: () => { calls++; return read.promise; } });
  const loading = c.load(); c.dispose(); read.resolve(page()); await loading;
  assert.equal(c.state.page, null);
  assert.equal(await c.play(1), false);
  assert.equal(await c.load(), false);
  assert.equal(calls, 1);
});

test('legacy responses remain readable without enabling unprotected writes', async () => {
  let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return page(0, 2, null); } });
  assert.equal(await c.load(), true);
  assert.equal(c.state.page.items.length, 2);
  assert.equal(await c.play(1), false);
  assert.equal(calls, 1);
});

test('empty, missing or sentinel renderer identities never start playback', async () => {
  for (const uri of [null, undefined, '', ' ', 'NOT_IMPLEMENTED']) {
    let calls = 0;
    const p = page(); p.items[0].uri = uri;
    const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return p; } });
    await c.load(); assert.equal(await c.play(1), false); assert.equal(calls, 1);
  }
});

test('malformed page counters and positions fail closed', async () => {
  for (const p of [null, {}, { ...page(), total: -1 }, { ...page(), items: [] },
    { ...page(), items: [{ position: 2, title: 'shifted', uri: 'a' }] },
    { ...page(), items: [{ position: 1, title: {}, uri: 'a' }] }]) {
    const c = QueueRemote.model({ zone: 'Room', call: async () => p });
    assert.equal(await c.load(), false);
    assert.equal(c.state.page, null);
    assert.equal(await c.play(1), false);
  }
});

test('invalid offsets make no request, empty and short pages remain valid', async () => {
  let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return page(0, 0); } });
  for (const offset of [-1, 1.5, NaN, 4294967295]) assert.equal(await c.load(offset), false);
  assert.equal(calls, 0);
  assert.equal(await c.load(), true);
  const short = QueueRemote.model({ zone: 'Room', call: async () => ({ ...page(), items: page().items.slice(0, 5) }) });
  assert.equal(await short.load(), true);
  assert.equal(short.state.page.items.length, 5);
});
