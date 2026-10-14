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

// The client drives a revisioned queue fixture; the production server remains
// responsible for native SOAP, policy and coordinator checks.
function movingQueue(total = 60, options = {}) {
  let revision = 'q:before';
  const entries = Array.from({ length: total }, (_, i) => ({ title: `Entry ${i + 1}`, uri: `renderer:${i + 1}` }));
  const calls = [];
  const c = QueueRemote.model({ zone: options.zone || 'Room@S2', played: options.played,
    call: async (name, args) => {
      calls.push(plain([name, args]));
      if (name === 'queue') {
        if (options.read) return options.read(args, revision);
        const offset = args.query.offset;
        return { total: entries.length, revision, playing: 2,
          items: entries.slice(offset, offset + args.query.limit).map((item, i) => ({ ...item, position: offset + i + 1 })) };
      }
      assert.equal(name, 'queueMove', 'no playback, remove, enqueue or fallback mutation');
      const b = args.body;
      assert.equal(b.expected_revision, revision);
      const start = b.position - 1, count = b.expected_uris.length;
      assert.deepEqual(plain(b.expected_uris), entries.slice(start, start + count).map((e) => e.uri));
      const block = entries.splice(start, count);
      const at = b.insert_before < b.position ? b.insert_before - 1 : b.insert_before - count - 1;
      entries.splice(at, 0, ...block);
      revision += ':next';
      const result = { done: 'Moved, not undoable', zone: 'Room', position: at + 1, count,
        total: entries.length, revision, changed: true, recovered: false, undoable: false };
      return options.reply ? options.reply(result) : result;
    } });
  return { c, entries, calls };
}

test('queue move reviews and preserves exact duplicate identities and the qualified destination', async () => {
  const { c, entries, calls } = movingQueue(10, { zone: "Room / ' @S1" });
  entries[1].uri = entries[2].uri = ' renderer:%2Fone,two?x=1&y=2 ';
  entries[1].title = '<img src=x onerror=alert(1)>';
  assert.equal(calls.length, 0);
  await c.load(); c.select(3, true); c.select(2, true);
  assert.equal(await c.move(), false);
  assert.equal(c.reviewMove(9), true);
  const reviewed = c.state.pending;
  assert.equal(reviewed.position, 7);
  assert.equal(Object.isFrozen(reviewed), true);
  assert.equal(Object.isFrozen(reviewed.body.expected_uris), true);
  assert.equal(calls.length, 1, 'review never dispatches a write');
  assert.equal(await c.play(1), false);
  assert.equal(await c.add('spotify:track:Example'), false);
  assert.equal(await c.remove(), false, 'a move review cannot authorize deletion');
  assert.equal(await c.move(reviewed), true);
  assert.deepEqual(calls[1], ['queueMove', { body: { zone: "Room / ' @S1", position: 2,
    insert_before: 9, expected_uris: [' renderer:%2Fone,two?x=1&y=2 ', ' renderer:%2Fone,two?x=1&y=2 '],
    expected_revision: 'q:before' } }]);
  assert.deepEqual(entries.map((e) => e.title), ['Entry 1', 'Entry 4', 'Entry 5', 'Entry 6', 'Entry 7', 'Entry 8',
    '<img src=x onerror=alert(1)>', 'Entry 3', 'Entry 9', 'Entry 10']);
  assert.equal(c.state.selected.size, 0);
  assert.equal(c.state.pending, null);
  assert.equal(c.state.page.revision, 'q:before:next');
});

test('all blocks and destinations in a small queue retain original-coordinate semantics', async () => {
  for (let first = 1; first <= 6; first++) for (let count = 1; count <= 7 - first; count++) {
    for (let before = 1; before <= 7; before++) {
      const { c, entries, calls } = movingQueue(6);
      const original = entries.map((e) => e.uri);
      const selected = original.slice(first - 1, first - 1 + count);
      const expected = original.filter((_, i) => i < first - 1 || i >= first - 1 + count);
      await c.load();
      for (let i = first; i < first + count; i++) c.select(i, true);
      const noop = before >= first && before <= first + count;
      assert.equal(c.reviewMove(before), !noop, `${first}, ${count}, ${before}`);
      if (noop) {
        assert.equal(await c.move(), false);
        assert.equal(calls.length, 1);
        assert.match(c.state.message, /already/);
        continue;
      }
      const at = before < first ? before - 1 : before - count - 1;
      expected.splice(at, 0, ...selected);
      assert.equal(c.state.pending.position, at + 1);
      assert.equal(await c.move(), true);
      assert.deepEqual(entries.map((e) => e.uri), expected);
      assert.equal(calls.length, 3);
    }
  }
});

test('move to end follows the moved block to its new page without changing playback', async () => {
  const { c, calls } = movingQueue();
  await c.load(); c.select(2, true); c.select(3, true);
  c.reviewMove(61);
  assert.equal(await c.move(), true);
  assert.deepEqual(calls.filter(([route]) => route === 'queue').map(([, args]) => args.query.offset), [0, 50]);
  assert.equal(c.state.page.items.find((e) => e.position === 59).uri, 'renderer:2');
  assert.equal(c.state.page.items.find((e) => e.position === 60).uri, 'renderer:3');
});

test('a source on a later page moves to the beginning with its absolute positions', async () => {
  const { c, calls } = movingQueue();
  await c.load(25); c.select(26, true); c.select(27, true); c.reviewMove(1);
  assert.equal(await c.move(), true);
  assert.equal(calls[1][1].body.position, 26);
  assert.equal(c.state.offset, 0);
  assert.deepEqual(plain(c.state.page.items.slice(0, 2).map((i) => i.uri)), ['renderer:26', 'renderer:27']);
});

test('a second deliberate move uses a fresh read revision, not the earlier selection', async () => {
  const { c, calls } = movingQueue(10);
  await c.load(); c.select(2, true); c.reviewMove(11); await c.move();
  c.select(10, true); c.reviewMove(1); await c.move();
  assert.deepEqual(calls.filter(([n]) => n === 'queueMove').map(([, a]) => a.body.expected_revision),
    ['q:before', 'q:before:next']);
  assert.equal(c.state.page.items[0].uri, 'renderer:2');
});

test('invalid destinations invalidate earlier move consent without dispatch', async () => {
  for (const before of [0, -1, 1.5, NaN, Infinity, '5', null, undefined, 12, 4294967296]) {
    const { c, calls } = movingQueue(10);
    await c.load(); c.select(2, true); c.reviewMove(1);
    assert.equal(c.reviewMove(before), false);
    assert.equal(c.state.pending, null);
    assert.equal(await c.move(), false);
    assert.equal(calls.length, 1);
  }
});

test('gaps, unavailable identities and legacy pages cannot authorize queue moves', async () => {
  for (const p of [page(0, 3, null), { ...page(0, 3), items: [{ position: 1, title: 'Unknown', uri: 'NOT_IMPLEMENTED' }] }]) {
    let calls = 0;
    const c = QueueRemote.model({ zone: 'Room', call: async () => { calls++; return p; } });
    await c.load(); c.select(1, true);
    assert.equal(c.reviewMove(4), false);
    assert.equal(await c.move(), false);
    assert.equal(calls, 1);
  }
  const { c, calls } = movingQueue();
  await c.load(); c.select(1, true); c.select(3, true);
  assert.equal(c.reviewMove(61), false);
  assert.equal(calls.length, 1);
});

test('removal and movement confirmations cannot be interchanged or revived', async () => {
  const { c, calls } = movingQueue();
  await c.load(); c.select(2, true); c.reviewMove(1);
  const move = c.state.pending;
  c.reviewRemoval();
  const removal = c.state.pending;
  assert.equal(await c.move(move), false);
  c.reviewMove(61);
  assert.equal(await c.remove(removal), false);
  assert.equal(await c.move(move), false, 'old same-kind buttons cannot confirm a new destination');
  c.cancelRemoval();
  assert.equal(await c.move(), false);
  assert.equal(calls.length, 1);
});

test('selection changes, refreshes and closing discard pending move consent', async () => {
  for (const change of [(c) => c.select(3, true), (c) => c.load(), (c) => c.close(), (c) => c.dispose()]) {
    const { c, calls } = movingQueue();
    await c.load(); c.select(2, true); c.reviewMove(1);
    const reviewed = c.state.pending;
    await change(c);
    assert.equal(c.state.pending, null);
    assert.equal(await c.move(reviewed), false);
    assert.equal(calls.filter(([n]) => n !== 'queue').length, 0);
  }
});

test('double confirmation cannot overlap a move with another queue action', async () => {
  const waiting = deferred();
  const { c, calls } = movingQueue(10, { reply: () => waiting.promise });
  await c.load(); c.select(2, true); c.reviewMove(1);
  const first = c.move();
  assert.equal(await c.move(), false);
  assert.equal(await c.remove(), false);
  assert.equal(await c.play(1), false);
  assert.equal(await c.add('spotify:track:Example'), false);
  assert.equal(await c.load(), false);
  assert.equal(c.reviewMove(5), false);
  waiting.resolve({ done: 'Moved', zone: 'Room', position: 1, count: 1, total: 10,
    revision: 'q:after', changed: true, recovered: false, undoable: false });
  assert.equal(await first, true);
  assert.equal(calls.filter(([n]) => n === 'queueMove').length, 1);
});

test('lost replies and stale selections leave moves uncertain and never repeat the request', async () => {
  for (const error of [new Error('lost reply'), Object.assign(new Error('queue changed'), { hint: 'Read the queue again.' })]) {
    const { c, calls } = movingQueue(10, { reply: () => { throw error; } });
    await c.load(); c.select(2, true); c.reviewMove(1);
    assert.equal(await c.move(), false);
    assert.equal(c.state.page, null);
    assert.equal(c.reviewMove(1), false);
    assert.equal(await c.move(), false);
    assert.deepEqual(calls.map(([n]) => n), ['queue', 'queueMove']);
    assert.match(c.state.message, /not repeated/);
    if (error.hint) assert.ok(c.state.message.includes(error.hint));
  }
});

test('incomplete and contradictory move acknowledgements never enable another edit', async () => {
  const bad = [() => null, () => ({}),
    (r) => ({ ...r, position: 2 }), (r) => ({ ...r, count: 0 }), (r) => ({ ...r, total: 9 }),
    (r) => ({ ...r, revision: '' }), (r) => ({ ...r, revision: 'q:before' }),
    (r) => ({ ...r, changed: false }), (r) => ({ ...r, changed: 'true' }),
    (r) => ({ ...r, recovered: undefined }), (r) => ({ ...r, undoable: true }),
    (r) => ({ ...r, done: null }), (r) => ({ ...r, zone: null })];
  for (const reply of bad) {
    const { c, calls } = movingQueue(10, { reply });
    await c.load(); c.select(2, true); c.reviewMove(1);
    assert.equal(await c.move(), false);
    assert.equal(c.state.page, null);
    assert.deepEqual(calls.map(([n]) => n), ['queue', 'queueMove']);
  }
});

test('server read-back recovery is accepted without a second move', async () => {
  const { c, calls } = movingQueue(10, { reply: (r) => ({ ...r, recovered: true,
    notes: [{ detail: 'Verified after a lost speaker response.' }] }) });
  await c.load(); c.select(2, true); c.reviewMove(1);
  assert.equal(await c.move(), true);
  assert.match(c.state.message, /Verified/);
  assert.equal(calls.filter(([n]) => n === 'queueMove').length, 1);
});

test('closing while reading discards the old page and reopening reads anew', async () => {
  const waiting = deferred(); let reads = 0;
  const c = QueueRemote.model({ zone: 'Room', call: () => ++reads === 1 ? waiting.promise : Promise.resolve(page()) });
  const old = c.load(); c.close();
  assert.equal(await c.load(), true);
  waiting.resolve(page(0, 10, 'obsolete')); await old;
  assert.equal(c.state.page.revision, 'observed revision');
});

test('closing a sent move does not abort or replay it and does not fetch a hidden page', async () => {
  const waiting = deferred();
  const { c, calls } = movingQueue(10, { reply: () => waiting.promise });
  await c.load(); c.select(2, true); c.reviewMove(1);
  const sent = c.move(); c.close();
  waiting.resolve({ done: 'Moved', zone: 'Room', position: 1, count: 1, total: 10,
    revision: 'q:after', changed: true, recovered: false, undoable: false });
  assert.equal(await sent, true);
  assert.equal(c.state.page, null);
  assert.deepEqual(calls.map(([n]) => n), ['queue', 'queueMove']);
  await c.load();
  assert.equal(c.state.page.items[0].uri, 'renderer:2');
});

test('a status callback failure cannot turn a confirmed move into an unhandled retry', async () => {
  for (const played of [() => { throw new Error('read failed'); }, () => Promise.reject(new Error('read failed'))]) {
    const { c, calls } = movingQueue(10, { played });
    await c.load(); c.select(2, true); c.reviewMove(1);
    assert.equal(await c.move(), true);
    assert.equal(c.state.page.items[0].uri, 'renderer:2');
    assert.deepEqual(calls.map(([n]) => n), ['queue', 'queueMove', 'queue']);
  }
});

test('disposing during a sent move prevents further reads and view publication', async () => {
  const waiting = deferred();
  const { c, calls } = movingQueue(10, { reply: () => waiting.promise });
  await c.load(); c.select(2, true); c.reviewMove(1);
  const sent = c.move(); c.dispose();
  waiting.resolve({ done: 'Moved', zone: 'Room', position: 1, count: 1, total: 10,
    revision: 'q:after', changed: true, recovered: false, undoable: false });
  assert.equal(await sent, true);
  assert.equal(await c.load(), false);
  assert.equal(c.state.page, null);
  assert.deepEqual(calls.map(([n]) => n), ['queue', 'queueMove']);
});

test('the shipped fetch client posts queue moves as JSON through its declared route', async (t) => {
  const http = require('node:http');
  const { once } = require('node:events');
  const received = [];
  const server = http.createServer(async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    received.push({ url: req.url, method: req.method, type: req.headers['content-type'], body });
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.method === 'GET' ? page(0, 10) : { done: 'Moved', zone: 'Room', position: 1,
      count: 1, total: 10, revision: 'q:after', changed: true, recovered: false, undoable: false }));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const origin = `http://127.0.0.1:${server.address().port}`;
  const source = readFileSync(join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  assert.match(source, /\nstart\(\);\s*$/);
  const api = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '\n({ call, path, ROUTES });'), {
    fetch: (path, init) => fetch(new URL(path, origin), init), URLSearchParams, AbortController, setTimeout, clearTimeout,
  });
  assert.deepEqual(plain(api.ROUTES.queueMove), ['POST', '/queue/move']);
  const c = QueueRemote.model({ zone: "Room / ' @S2", call: api.call });
  await c.load(); c.select(2, true); c.reviewMove(1);
  assert.equal(await c.move(), true);
  assert.equal(received.length, 3);
  assert.equal(received[1].method, 'POST');
  assert.equal(received[1].url, '/queue/move');
  assert.equal(received[1].type, 'application/json');
  assert.deepEqual(JSON.parse(received[1].body), { zone: "Room / ' @S2", position: 2, insert_before: 1,
    expected_uris: ['renderer:2'], expected_revision: 'observed revision' });
  assert.match(received[0].url, /Room%20%2F%20'%20%40S2\/queue\?offset=0&limit=25$/);
});
