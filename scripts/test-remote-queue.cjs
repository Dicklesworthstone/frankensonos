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

test('removal requires review and sends exactly the ordered selected range', async () => {
  const calls = [];
  const p = page(0, 10); p.items[2].uri = p.items[1].uri;
  const c = QueueRemote.model({ zone: 'Room@S1', call: async (name, args) => {
    calls.push([name, args]);
    return name === 'queue' ? p : { done: 'Removed, not undoable', position: 2, count: 2, remaining: 8, undoable: false, recovered: true };
  } });
  await c.load();
  c.select(3, true); c.select(2, true);
  assert.equal(await c.remove(), false, 'selection alone is not consent');
  assert.equal(c.reviewRemoval(), true);
  assert.equal(calls.length, 1, 'review is read-only');
  assert.equal(await c.play(1), false, 'no conflicting action during review');
  assert.equal(await c.remove(), true);
  assert.deepEqual(plain(calls[1]), ['queueRemove', { body: {
    zone: 'Room@S1', position: 2, expected_uris: [p.items[1].uri, p.items[1].uri], expected_revision: p.revision,
  } }]);
  assert.equal(c.state.selected.size, 0);
  assert.equal(c.state.pending, null);
});

test('a gap or an invalid selection cannot produce a deletion', async () => {
  let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return page(); } });
  await c.load(); c.select(1, true); c.select(3, true);
  assert.equal(c.removable(), null);
  assert.equal(c.reviewRemoval(), false);
  assert.equal(await c.remove(), false);
  assert.equal(c.select(99, true), false);
  assert.equal(calls, 1);
});

test('cancelled, edited and refreshed confirmations cannot remove anything', async () => {
  const calls = [];
  const c = QueueRemote.model({ zone: 'Room', call: async (name, args) => { calls.push(name); return page(args.query.offset); } });
  await c.load(); c.select(1, true); c.reviewRemoval(); c.cancelRemoval();
  assert.equal(await c.remove(), false);
  c.reviewRemoval(); c.select(2, true);
  assert.equal(c.state.pending, null);
  assert.equal(await c.remove(), false);
  c.reviewRemoval(); await c.load(25);
  assert.equal(c.state.selected.size, 0);
  assert.equal(c.state.pending, null);
  assert.equal(await c.remove(), false);
  assert.deepEqual(calls, ['queue', 'queue']);
});

test('a failed removal cannot be retried by a second click or confirmation', async () => {
  const waiting = deferred(); const calls = [];
  const c = QueueRemote.model({ zone: 'Room', call: async (name) => {
    calls.push(name); return name === 'queue' ? page() : waiting.promise;
  } });
  await c.load(); c.select(1, true); c.reviewRemoval(); const removal = c.remove();
  assert.equal(await c.remove(), false);
  assert.equal(c.reviewRemoval(), false);
  assert.equal(await c.load(), false);
  waiting.reject(new Error('reply lost')); assert.equal(await removal, false);
  assert.equal(await c.remove(), false);
  assert.equal(c.state.page, null);
  assert.match(c.state.message, /not repeated/);
  assert.deepEqual(calls, ['queue', 'queueRemove']);
});

test('invalid removal success bodies are uncertain, never silently accepted', async () => {
  const good = { done: 'Removed', position: 1, count: 1, remaining: 59, recovered: false, undoable: false };
  for (const answer of [null, {}, { ...good, position: 2 }, { ...good, count: 2 },
    { ...good, remaining: 100 }, { ...good, undoable: true }, { ...good, recovered: undefined }]) {
    let writes = 0;
    const c = QueueRemote.model({ zone: 'Room', call: async (name) => {
      if (name === 'queue') return page(); writes++; return answer;
    } });
    await c.load(); c.select(1, true); c.reviewRemoval();
    assert.equal(await c.remove(), false);
    assert.equal(writes, 1);
    assert.equal(c.state.page, null);
  }
});

test('deleting a final page navigates back within the remaining queue', async () => {
  const offsets = [];
  const c = QueueRemote.model({ zone: 'Room', call: async (name, args) => {
    if (name === 'queue') { offsets.push(args.query.offset); return page(args.query.offset, offsets.length === 1 ? 26 : 25); }
    return { done: 'Removed', position: 26, count: 1, remaining: 25, undoable: false, recovered: false };
  } });
  await c.load(25); c.select(26, true); c.reviewRemoval();
  assert.equal(await c.remove(), true);
  assert.deepEqual(offsets, [25, 0]);
  assert.equal(c.state.offset, 0);
});

test('disposing a reviewed card prevents writes and pending-write completion causes no new read', async () => {
  let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return page(); } });
  await c.load(); c.select(1, true); c.reviewRemoval(); c.dispose();
  assert.equal(await c.remove(), false); assert.equal(calls, 1);
  const waiting = deferred(); const seen = [];
  const d = QueueRemote.model({ zone: 'Room', call: async (name) => { seen.push(name); return name === 'queue' ? page() : waiting.promise; } });
  await d.load(); d.select(1, true); d.reviewRemoval(); const removal = d.remove(); d.dispose();
  waiting.resolve({ done: 'Removed', position: 1, count: 1, remaining: 59, undoable: false, recovered: false });
  await removal; assert.deepEqual(seen, ['queue', 'queueRemove']);
});

test('missing revisions or missing identities keep removal read-only', async () => {
  for (const p of [page(0, 2, null), { ...page(), items: [{ position: 1, title: 'Unknown', uri: null }] }]) {
    let calls = 0;
    const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return p; } });
    await c.load(); assert.equal(c.select(1, true), false); assert.equal(c.reviewRemoval(), false);
    assert.equal(await c.remove(), false); assert.equal(calls, 1);
  }
});

test('Spotify addition forwards the source and placement once and preserves the result', async () => {
  for (const next of [false, true]) {
    const calls = [];
    const c = QueueRemote.model({ zone: 'Room@S2', call: async (name, args) => {
      calls.push([name, args]);
      return name === 'queue' ? page(0, 0) : { done: 'Queued album; not playing from queue', first: 1, count: 3, playing_queue: false };
    } });
    await c.load(); c.state.source = 'spotify:album:Example';
    assert.equal(await c.add(' spotify:album:Example ', next), true);
    assert.deepEqual(plain(calls[1]), ['queueAdd', { body: { zone: 'Room@S2', source_uri: 'spotify:album:Example', next } }]);
    assert.equal(c.state.source, '');
    assert.match(c.state.message, /not playing/);
    assert.equal(calls.length, 3, 'never sends Play');
  }
});

test('blank input and pending removal do not enqueue', async () => {
  let calls = 0;
  const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return page(); } });
  await c.load();
  for (const source of ['', ' ', null, undefined]) assert.equal(await c.add(source), false);
  c.select(1, true); c.reviewRemoval();
  assert.equal(await c.add('spotify:track:Example'), false);
  assert.equal(calls, 1);
});

test('an uncertain addition preserves the draft but requires a fresh read', async () => {
  const waiting = deferred(); const calls = [];
  const c = QueueRemote.model({ zone: 'Room', call: async (name) => {
    calls.push(name); return name === 'queue' ? page() : waiting.promise;
  } });
  await c.load(); c.state.source = 'spotify:playlist:Example';
  const writing = c.add(c.state.source);
  assert.equal(await c.add(c.state.source), false);
  waiting.reject(new Error('reply lost')); assert.equal(await writing, false);
  assert.equal(c.state.source, 'spotify:playlist:Example');
  assert.equal(await c.add(c.state.source), false);
  assert.deepEqual(calls, ['queue', 'queueAdd']);
});

test('malformed enqueue acknowledgements do not authorize another write', async () => {
  for (const result of [null, {}, { done: 'Queued', first: 0, count: 1, playing_queue: true },
    { done: 'Queued', first: 1, count: -1, playing_queue: true }, { done: 'Queued', first: 1, count: 1 }]) {
    const calls = [];
    const c = QueueRemote.model({ zone: 'Room', call: async (name) => { calls.push(name); return name === 'queue' ? page() : result; } });
    await c.load(); assert.equal(await c.add('spotify:track:Example'), false);
    assert.equal(c.state.page, null); assert.deepEqual(calls, ['queue', 'queueAdd']);
  }
});
