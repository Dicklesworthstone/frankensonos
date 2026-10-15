// node --test scripts/test-remote-live.cjs; no packages or network required.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
// Omit only automatic page mounting; execute the shipped controller unchanged.
const live = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\nLiveRemote;', { setTimeout, clearTimeout });
const settle = async () => { for (let n = 0; n < 30; n++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function clock() {
  let now = 0, id = 0;
  const timers = new Map();
  return {
    schedule(fn, delay) { timers.set(++id, { fn, at: now + delay }); return id; },
    cancel(key) { timers.delete(key); },
    async tick(ms) {
      const end = now + ms;
      for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
    get pending() { return timers.size; },
    get delays() { return [...timers.values()].map((t) => t.at - now); },
  };
}
class Feed {
  readyState = 0;
  closed = 0;
  handlers = new Map();
  addEventListener(name, callback) { this.handlers.set(name, callback); }
  emit(name, data) {
    if (name === 'open') this.readyState = 1;
    this.handlers.get(name)?.({ data: typeof data === 'string' ? data : JSON.stringify(data) });
  }
  close() { this.closed++; this.readyState = 2; }
}
function harness(options = {}) {
  const time = clock(), feeds = [], reads = [], labels = [];
  const controller = live.model({
    connect: () => { const f = new Feed(); feeds.push(f); return f; },
    reload: async () => { reads.push('all'); },
    refresh: async (room) => { reads.push(room); },
    status: (label) => labels.push(label),
    ...options, schedule: time.schedule, cancel: time.cancel,
  });
  return { time, feeds, reads, labels, controller };
}
async function opened(h) {
  h.controller.resume(); h.feeds[0].emit('open'); await h.time.tick(300);
}

test('initial subscription and every reconnect reconcile a full snapshot', async () => {
  const h = harness(); await opened(h);
  assert.deepEqual(h.reads, ['all']);
  assert.equal(h.labels.at(-1), 'Live');
  h.feeds[0].readyState = 0; h.feeds[0].emit('error');
  h.feeds[0].emit('open'); await h.time.tick(300);
  assert.deepEqual(h.reads, ['all', 'all']);
  assert.equal(h.feeds.length, 1, 'CONNECTING feeds keep browser resume state');
});

test('a healthy quiet stream does not poll the household', async () => {
  const h = harness(); await opened(h); await h.time.tick(180000);
  assert.deepEqual(h.reads, ['all']); assert.equal(h.time.pending, 0);
});

test('without EventSource full reads repeat only after the previous one settles', async () => {
  const h = harness({ connect: null }); h.controller.resume();
  await h.time.tick(300); assert.deepEqual(h.reads, ['all']);
  await h.time.tick(10000); assert.equal(h.reads.length, 1);
  await h.time.tick(300); assert.deepEqual(h.reads, ['all', 'all']);
  assert.equal(h.labels.at(-1), 'Refreshing without live events');
});

test('dropped event connections get fallback reads while native reconnect proceeds', async () => {
  const h = harness(); await opened(h);
  h.feeds[0].readyState = 0; h.feeds[0].emit('error');
  await h.time.tick(10300);
  assert.deepEqual(h.reads, ['all', 'all']); assert.equal(h.feeds.length, 1);
  h.feeds[0].emit('open'); await h.time.tick(300); await h.time.tick(60000);
  assert.equal(h.reads.length, 3); assert.equal(h.time.pending, 0);
});

test('a permanently CLOSED event stream is replaced at the bounded retry pace', async () => {
  const h = harness(); await opened(h);
  h.feeds[0].readyState = 2; h.feeds[0].emit('error');
  assert.equal(h.feeds[0].closed, 1); assert.equal(h.feeds.length, 1);
  await h.time.tick(10000); assert.equal(h.feeds.length, 2);
  h.feeds[1].emit('open'); await h.time.tick(300);
  assert.equal(h.labels.at(-1), 'Live');
});

test('constructor failures leave fallback reads usable', async () => {
  let attempts = 0;
  const h = harness({ connect: () => { attempts++; throw new Error('unavailable'); } });
  h.controller.resume(); await h.time.tick(10600);
  assert.equal(attempts, 2); assert.deepEqual(h.reads, ['all', 'all']);
});

test('repeated error events do not postpone or multiply a fallback deadline', async () => {
  const h = harness(); await opened(h); h.feeds[0].readyState = 0;
  for (let n = 0; n < 20; n++) { h.feeds[0].emit('error'); await h.time.tick(500); }
  await h.time.tick(300); assert.equal(h.reads.length, 2);
});

test('notification bursts coalesce by room with a fixed non-starving deadline', async () => {
  const h = harness(); await opened(h);
  for (let n = 0; n < 3; n++) { h.feeds[0].emit('zone.state', { room: 'Den', volume: n }); await h.time.tick(100); }
  assert.deepEqual(h.reads, ['all', 'Den']);
  h.feeds[0].emit('dj.status', { zone: 'Den' }); h.feeds[0].emit('zone.state', { room: 'Den', mute: true });
  await h.time.tick(300); assert.deepEqual(h.reads, ['all', 'Den', 'Den']);
});

test('topology and reset events supersede pending room reads', async () => {
  const h = harness(); await opened(h);
  for (const kind of ['topology.changed', 'player.health', 'events.reset']) {
    h.feeds[0].emit('zone.state', { room: 'Den' }); h.feeds[0].emit(kind, {});
    h.feeds[0].emit('dj.status', { zone: 'Other' }); await h.time.tick(300);
  }
  assert.deepEqual(h.reads, ['all', 'all', 'all', 'all']);
});

test('malformed or missing event identities cause a fresh read, not guessed deltas', async () => {
  const h = harness(); await opened(h);
  for (const data of ['{', 'null', '[]', '{}', '{"room":42}', '{"room":""}']) {
    h.feeds[0].emit('zone.state', data); await h.time.tick(300);
  }
  assert.equal(h.reads.length, 7); assert.ok(h.reads.every((r) => r === 'all'));
});

test('pending-room storage is bounded even for unknown event names', async () => {
  const h = harness(); await opened(h);
  for (let n = 0; n < 1000; n++) h.feeds[0].emit('zone.state', { room: `Room ${n}` });
  await h.time.tick(300); assert.deepEqual(h.reads, ['all', 'all']);
});

test('events during a slow read cause one later read without overlap', async () => {
  const waiting = deferred(); let ongoing = 0, peak = 0, calls = 0;
  const h = harness({ refresh: async () => { calls++; peak = Math.max(peak, ++ongoing); await waiting.promise; ongoing--; } });
  await opened(h); h.feeds[0].emit('zone.state', { room: 'Den' }); await h.time.tick(300);
  for (let n = 0; n < 30; n++) h.feeds[0].emit('zone.state', { room: 'Den' });
  await h.time.tick(30000); assert.equal(calls, 1);
  waiting.resolve(); await settle(); await h.time.tick(300);
  assert.equal(calls, 2); assert.equal(peak, 1);
});

test('a failed read backs off and recovers with a full snapshot', async () => {
  let attempts = 0;
  const h = harness({ reload: async () => { if (++attempts < 4) throw new Error('offline'); } });
  h.controller.resume(); h.feeds[0].emit('open'); await h.time.tick(300);
  assert.equal(h.labels.at(-1), 'State refresh failed; retrying…');
  assert.deepEqual(h.time.delays, [20000]);
  await h.time.tick(20300); assert.deepEqual(h.time.delays, [40000]);
  await h.time.tick(40300); assert.deepEqual(h.time.delays, [60000]);
  await h.time.tick(60300); assert.equal(h.labels.at(-1), 'Live'); assert.equal(h.time.pending, 0);
});

test('notifications after read failure respect backoff instead of causing a retry storm', async () => {
  let calls = 0;
  const h = harness({ reload: async () => { calls++; throw new Error('unreachable'); } });
  h.controller.resume(); h.feeds[0].emit('open'); await h.time.tick(300);
  for (let n = 0; n < 10; n++) { h.feeds[0].emit('zone.state', { room: 'Den' }); await h.time.tick(1000); }
  assert.equal(calls, 1); await h.time.tick(10300); assert.equal(calls, 2);
});

test('partial refresh failures are repaired with a full read', async () => {
  const h = harness({ refresh: async () => { throw new Error('room unavailable'); } });
  await opened(h); h.feeds[0].emit('zone.state', { room: 'Den' }); await h.time.tick(300);
  await h.time.tick(20300); assert.deepEqual(h.reads, ['all', 'all']);
  assert.equal(h.labels.at(-1), 'Live');
});

test('one failed room refresh does not starve later rooms in the same live batch', async () => {
  const rooms = [];
  const h = harness({
    refresh: async (room) => {
      rooms.push(room);
      if (room === 'A') throw new Error('A is unreachable');
    },
  });
  await opened(h);
  h.feeds[0].emit('zone.state', { room: 'A' });
  h.feeds[0].emit('zone.state', { room: 'B' });
  h.feeds[0].emit('zone.state', { room: 'C' });
  await h.time.tick(300);
  assert.deepEqual(rooms, ['A', 'B', 'C']);
  assert.equal(h.labels.at(-1), 'State refresh failed; retrying…');
  assert.deepEqual(h.time.delays, [20000]);

  await h.time.tick(20300);
  assert.deepEqual(h.reads, ['all', 'all']);
  assert.equal(h.labels.at(-1), 'Live');
});

test('suspension closes the stream and stops timers; resumption reconciles anew', async () => {
  const h = harness(); await opened(h);
  h.feeds[0].emit('zone.state', { room: 'Den' }); h.controller.suspend();
  assert.equal(h.feeds[0].closed, 1); assert.equal(h.time.pending, 0);
  await h.time.tick(120000); assert.deepEqual(h.reads, ['all']);
  h.controller.resume(); h.feeds[1].emit('open'); await h.time.tick(300);
  assert.deepEqual(h.reads, ['all', 'all']);
});

test('old feeds cannot trigger updates after resume or disposal', async () => {
  const h = harness(); await opened(h); h.controller.suspend(); h.controller.resume();
  h.feeds[1].emit('open'); await h.time.tick(300);
  h.feeds[0].emit('open'); h.feeds[0].emit('events.reset', {}); await h.time.tick(300);
  assert.equal(h.reads.length, 2);
  h.controller.dispose(); h.controller.resume(); h.controller.refresh(); await h.time.tick(60000);
  assert.equal(h.time.pending, 0); assert.equal(h.feeds.length, 2);
});

test('reconnect during an old read cannot report a synchronized baseline early', async () => {
  const waiting = deferred(); let reads = 0;
  const h = harness({ reload: async () => { if (++reads === 1) await waiting.promise; } });
  h.controller.resume(); await h.time.tick(300); h.feeds[0].emit('open');
  waiting.resolve(); await settle(); assert.equal(h.labels.at(-1), 'Synchronizing…');
  await h.time.tick(300); assert.equal(reads, 2); assert.equal(h.labels.at(-1), 'Live');
});

test('suspending an active batch prevents remaining obsolete room reads', async () => {
  const waiting = deferred(), rooms = [];
  const h = harness({ refresh: async (r) => { rooms.push(r); await waiting.promise; } });
  await opened(h); h.feeds[0].emit('zone.state', { room: 'A' }); h.feeds[0].emit('zone.state', { room: 'B' });
  await h.time.tick(300); h.controller.suspend(); waiting.resolve(); await settle();
  assert.deepEqual(rooms, ['A']); assert.equal(h.time.pending, 0);
});

test('resume during an old read waits rather than starting overlapping work', async () => {
  const waiting = deferred(); let reads = 0;
  const h = harness({ reload: async () => { if (++reads === 1) await waiting.promise; } });
  h.controller.resume(); await h.time.tick(300); h.controller.suspend(); h.controller.resume();
  await h.time.tick(300); assert.equal(reads, 1);
  waiting.resolve(); await settle(); await h.time.tick(300); assert.equal(reads, 2);
});

test('same-name household events invalidate all matches instead of updating the first room', () => {
  const zones = [{ household: 'S1', members: ['Den'] }, { household: 'S2', members: ['Den', 'Bedroom'] }];
  assert.equal(live.affected(zones, 'Den').length, 2);
  assert.equal(live.affected(zones, 'Bedroom')[0], zones[1]);
  assert.equal(live.affected(zones, 'Unknown').length, 0);
});

test('resume is idempotent and manual refresh coalesces rather than opening duplicate feeds', async () => {
  const h = harness(); h.controller.resume(); h.controller.resume(); h.controller.refresh(); h.controller.refresh();
  await h.time.tick(300); assert.equal(h.feeds.length, 1); assert.deepEqual(h.reads, ['all']);
});

test('read requests have a deadline and an aborted fetch releases recovery', async () => {
  const time = clock(); let request;
  const ctx = { AbortController, URLSearchParams, setTimeout: time.schedule, clearTimeout: time.cancel,
    fetch: async (_, init) => { request = init; return new Promise((resolve, reject) => {
      init.signal.addEventListener('abort', () => reject(new Error('read aborted')));
    }); },
  };
  const call = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\ncall;', ctx);
  const result = call('zones');
  const rejected = assert.rejects(result, (error) => error.code === 'REMOTE_READ_TIMEOUT' && !error.uncertain);
  assert.equal(request.signal.aborted, false); await time.tick(15000); await rejected;
  assert.equal(request.signal.aborted, true); assert.equal(time.pending, 0);
});

test('read deadlines are removed after response parsing, including failures', async () => {
  for (const ok of [true, false]) {
    const time = clock(); let reads = 0;
    const call = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\ncall;', {
      AbortController, URLSearchParams, setTimeout: time.schedule, clearTimeout: time.cancel,
      fetch: async () => { reads++; return { ok, status: ok ? 200 : 503, text: async () => '[]' }; },
    });
    if (ok) await call('zones');
    else {
      const rejected = assert.rejects(call('zones'), (error) => error.status === 503);
      await settle(); await time.tick(2000); await rejected;
    }
    assert.equal(reads, ok ? 1 : 3);
    assert.equal(time.pending, 0);
  }
});

test('write failures release their separate deadline without replay or a false cancellation', async () => {
  const time = clock(); let writes = 0, request;
  const call = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\ncall;', {
    AbortController, URLSearchParams, setTimeout: time.schedule, clearTimeout: time.cancel,
    fetch: async (_, init) => { writes++; request = init; throw new Error('reply lost'); },
  });
  await assert.rejects(call('pause', { body: { zone: 'Den@S2' } }),
    (error) => error.code === 'REMOTE_OUTCOME_UNKNOWN' && error.uncertain && error.cause.message === 'reply lost');
  assert.equal(writes, 1); assert.equal(request.signal.aborted, false); assert.equal(time.pending, 0);
  await time.tick(120000); assert.equal(writes, 1); assert.equal(request.signal.aborted, false);
});
