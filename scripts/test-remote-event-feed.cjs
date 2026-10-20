// node --test scripts/test-remote-event-feed.cjs; no packages or speakers.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const F = require('../crates/fsonos-api/src/remote/event-feed.js');
const settle = async () => { for (let i = 0; i < 60; i++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function clock() {
  let now = 0, serial = 0;
  const timers = new Map();
  return {
    now: () => now,
    schedule(fn, delay) { assert.ok(delay >= 0 && delay <= 60000); timers.set(++serial, { fn, at: now + delay }); return serial; },
    cancel(id) { timers.delete(id); },
    get pending() { return timers.size; },
    async tick(ms) {
      const end = now + ms; await settle();
      for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}
function body() {
  const queued = [], waiting = [];
  let cancelled = 0;
  const reader = {
    read() { if (queued.length) return Promise.resolve(queued.shift()); const d = deferred(); waiting.push(d); return d.promise; },
    cancel() { cancelled++; return new Promise(() => {}); }, // Cancellation need not settle to release the caller.
  };
  return {
    response: { status: 200, headers: { get: (k) => k === 'content-type' ? 'text/event-stream; charset=utf-8' : null },
      body: { getReader: () => reader, cancel: reader.cancel } },
    send(text) { this.bytes(new TextEncoder().encode(text)); },
    bytes(value) { const item = { value, done: false }; const d = waiting.shift(); if (d) d.resolve(item); else queued.push(item); },
    end() { const item = { done: true }; const d = waiting.shift(); if (d) d.resolve(item); else queued.push(item); },
    fail(error) { const d = waiting.shift(); assert.ok(d); d.reject(error); },
    get cancelled() { return cancelled; },
  };
}
function harness(fetcher) {
  const time = clock(), requests = [], events = [];
  const feed = F.open('/events', { schedule: time.schedule, cancel: time.cancel, now: time.now,
    wallNow: time.now, random: () => 0, gate: { until: 0 }, fetcher: async (url, init) => {
      requests.push({ url, init, at: time.now() }); return fetcher(url, init, requests.length);
    } });
  for (const name of ['open', 'error', 'zone.state', 'events.reset', 'dj.status', 'message']) {
    feed.addEventListener(name, (event) => events.push(event));
  }
  return { time, requests, events, feed };
}

for (const newline of ['\n', '\r', '\r\n']) {
  test(`SSE ${JSON.stringify(newline)} framing, BOM and multiline data survive every chunk split`, () => {
    const input = `\ufeff: heartbeat${newline}${newline}id: 9007199254740993${newline}event: zone.state${newline}data: first${newline}data:  second${newline}${newline}`;
    for (let split = 0; split <= input.length; split++) {
      const events = [], cursors = []; let activity = 0;
      const p = F.parser({ event: e => events.push(e), cursor: id => cursors.push(id), activity: () => activity++ });
      p.push(input.slice(0, split)); p.push(input.slice(split));
      assert.deepEqual(events, [{ type: 'zone.state', data: 'first\n second', lastEventId: '9007199254740993' }]);
      assert.equal(cursors.at(-1), '9007199254740993'); assert.equal(activity, 2);
    }
  });
}

test('EOF fragments never dispatch or advance the committed cursor', () => {
  const events = [], cursors = [];
  const p = F.parser({ event: e => events.push(e), cursor: id => cursors.push(id) });
  p.push('id: 3\ndata: first\n\nid: 4\ndata: truncated\n');
  assert.deepEqual(events.map(e => e.data), ['first']); assert.deepEqual(cursors, ['3']);
});

test('comments and unknown fields do not dispatch; empty data does, and empty id clears', () => {
  const events = [], cursors = [];
  const p = F.parser({ event: e => events.push(e), cursor: id => cursors.push(id), lastId: '8' });
  p.push(': keepalive\nunknown: value\n\ndata\n\nid\n\ndata: last\n\n');
  assert.deepEqual(events, [
    { type: 'message', data: '', lastEventId: '8' }, { type: 'message', data: 'last', lastEventId: '' },
  ]);
  assert.deepEqual(cursors, ['8', '8', '', '']);
});

test('null-containing IDs are ignored and retry accepts only ASCII digits', () => {
  const events = [], retries = [];
  const p = F.parser({ lastId: '9', event: e => events.push(e), retry: n => retries.push(n) });
  p.push('id: 4\0bad\nretry: 12000\nretry: -1\nretry: 1.5\nretry: +3\nretry: ٣\ndata: a\n\n');
  assert.equal(events[0].lastEventId, '9'); assert.deepEqual(retries, [12000]);
});

for (const input of ['x'.repeat(65), 'data: x\n'.repeat(10), ': comment\n'.repeat(8)]) {
  test(`bounds incomplete event memory (${input.slice(0, 10)})`, () => {
    const p = F.parser({ event: () => assert.fail('no complete event'), limit: 64 });
    assert.throws(() => p.push(input), /receive limit/);
  });
}

test('completed heartbeat blocks release the frame budget', () => {
  let activity = 0;
  const p = F.parser({ event: () => assert.fail(), activity: () => activity++, limit: 32 });
  p.push(': heartbeat\n\n'.repeat(10000)); assert.equal(activity, 10000);
});

test('closing the parser from a listener stops later events in the same chunk', () => {
  const events = [];
  const p = F.parser({ event(e) { events.push(e); p.stop(); } });
  p.push('data: a\n\ndata: b\n\n'); assert.equal(events.length, 1);
});

test('cursor validation preserves u64 identity without numeric rounding', () => {
  for (const id of ['', '0', '9007199254740993', '18446744073709551615']) assert.equal(F.validId(id), true);
  for (const id of ['18446744073709551616', '-1', '+1', '1.0', 'abc', '1\n', '9'.repeat(21)]) assert.equal(F.validId(id), false);
});

test('the transport cannot be pointed at another endpoint or origin', () => {
  for (const url of ['/pause', '//example.invalid/events', '/events?wait=30']) assert.throws(() => F.open(url), /endpoint/);
});

test('handshake deadline releases even a fetch that ignores abort, then fences its late body', async () => {
  const held = deferred(), stale = body(), good = body();
  const h = harness((_, __, n) => n === 1 ? held.promise : good.response);
  await h.time.tick(14999); assert.equal(h.events.length, 0);
  await h.time.tick(1); assert.equal(h.events[0].type, 'error'); assert.equal(h.requests[0].init.signal.aborted, true);
  await h.time.tick(1000); assert.equal(h.requests.length, 2); assert.equal(h.feed.readyState, 1);
  held.resolve(stale.response); stale.send('data: obsolete\n\n'); await settle();
  assert.equal(stale.cancelled, 1); assert.equal(h.events.filter(e => e.type === 'message').length, 0);
  h.feed.close(); assert.equal(h.time.pending, 0);
});

test('a silent body expires after three heartbeat intervals, without awaiting reader cancellation', async () => {
  const first = body(), second = body();
  const h = harness((_, __, n) => n === 1 ? first.response : second.response);
  await settle(); assert.equal(h.feed.readyState, 1);
  await h.time.tick(44999); assert.equal(h.events.filter(e => e.type === 'error').length, 0);
  await h.time.tick(1); assert.equal(h.feed.readyState, 0); assert.equal(first.cancelled, 1);
  assert.equal(h.requests[0].init.signal.aborted, true);
  await h.time.tick(1000); assert.equal(h.requests.length, 2);
  first.send('id: 100\nevent: zone.state\ndata: obsolete\n\n');
  second.send('id: 1\nevent: zone.state\ndata: current\n\n'); await settle();
  assert.deepEqual(h.events.filter(e => e.type === 'zone.state').map(e => e.data), ['current']);
  h.feed.close(); assert.equal(h.time.pending, 0);
});

test('quiet heartbeat comments maintain one connection without snapshot events or polling', async () => {
  const stream = body(), h = harness(() => stream.response); await settle();
  for (let i = 0; i < 20; i++) { await h.time.tick(15000); stream.send(': heartbeat\n\n'); await settle(); }
  assert.equal(h.requests.length, 1); assert.deepEqual(h.events.map(e => e.type), ['open']);
  assert.equal(h.time.pending, 1); h.feed.close(); assert.equal(h.time.pending, 0);
});

test('fragment trickles do not keep an incomplete frame alive indefinitely', async () => {
  const stream = body(), h = harness(() => stream.response); await settle();
  for (let i = 0; i < 4; i++) { stream.send('data: partial'); await h.time.tick(10000); }
  await h.time.tick(5000); assert.equal(h.feed.readyState, 0); h.feed.close();
});

test('UTF-8 split across byte chunks is decoded before the SSE parser', async () => {
  const stream = body(), h = harness(() => stream.response); await settle();
  const bytes = new TextEncoder().encode('event: zone.state\ndata: Café 🎵\n\n');
  for (const byte of bytes) { stream.bytes(new Uint8Array([byte])); await settle(); }
  assert.equal(h.events.find(e => e.type === 'zone.state').data, 'Café 🎵'); h.feed.close();
});

test('reconnect uses only the last complete cursor and all requests stay read-only and same-origin', async () => {
  const streams = [body(), body()], h = harness((_, __, n) => streams[n - 1].response); await settle();
  streams[0].send('id: 9007199254740993\nevent: zone.state\ndata: {}\n\nid: 9007199254740994\ndata: partial\n');
  streams[0].end(); await settle(); await h.time.tick(1000);
  assert.equal(h.requests[1].init.headers['Last-Event-ID'], '9007199254740993');
  for (const { url, init } of h.requests) {
    assert.equal(url, '/events'); assert.equal(init.method, 'GET'); assert.equal(init.body, undefined);
    assert.equal(init.headers.Accept, 'text/event-stream'); assert.equal(init.credentials, 'same-origin');
    assert.equal(init.redirect, 'error'); assert.equal(init.cache, 'no-store');
  }
  h.feed.close();
});

test('journal reset clears an old daemon cursor even without a subsequent numbered event', async () => {
  const streams = [body(), body()], h = harness((_, __, n) => streams[n - 1].response); await settle();
  streams[0].send('id: 99\ndata: old\n\nevent: events.reset\ndata: {"oldest":1}\n\n: heartbeat\n\n');
  streams[0].end(); await settle(); await h.time.tick(1000);
  assert.equal(h.requests[1].init.headers['Last-Event-ID'], undefined);
  assert.equal(h.events.filter(e => e.type === 'events.reset').length, 1); h.feed.close();
});

for (const [name, payload] of [['oversized frame', 'x'.repeat(F.MAX_FRAME + 1)], ['invalid cursor', 'id: invalid\ndata: bad\n\n']]) {
  test(`${name} discards the resume cursor so the next subscription can recover`, async () => {
    const streams = [body(), body()], h = harness((_, __, n) => streams[n - 1].response); await settle();
    streams[0].send('id: 10\ndata: valid\n\n' + payload); await settle(); await h.time.tick(1000);
    assert.equal(h.requests.length, 2); assert.equal(h.requests[1].init.headers['Last-Event-ID'], undefined);
    h.feed.close();
  });
}

test('network failures use capped exponential retry, never concurrent fetch attempts', async () => {
  const h = harness(() => { throw new Error('offline'); });
  await settle();
  for (const ms of [1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000]) await h.time.tick(ms);
  assert.deepEqual(h.requests.map(r => r.at), [0, 1000, 3000, 7000, 15000, 31000, 63000, 123000, 183000]);
  assert.equal(h.time.pending, 1); h.feed.close(); assert.equal(h.time.pending, 0);
});

test('a healthy frame restores the short retry after an earlier network failure', async () => {
  const stream = body(), h = harness((_, __, n) => { if (n === 1) throw new Error('offline'); return stream.response; });
  await h.time.tick(1000); stream.send(': heartbeat\n\n'); stream.end(); await settle();
  await h.time.tick(999); assert.equal(h.requests.length, 2);
  await h.time.tick(1); assert.equal(h.requests.length, 3); h.feed.close();
});

test('server retry directives cannot turn failure into a tight retry loop', async () => {
  const streams = [body(), body()], h = harness((_, __, n) => streams[n - 1].response); await settle();
  streams[0].send('retry: 0\n\n'); streams[0].end(); await settle();
  await h.time.tick(999); assert.equal(h.requests.length, 1);
  await h.time.tick(1); assert.equal(h.requests.length, 2); h.feed.close();
});

test('server retry directives are retained across reconnects', async () => {
  const streams = [body(), body()], h = harness((_, __, n) => streams[n - 1].response); await settle();
  streams[0].send('retry: 90000\n\n'); streams[0].end(); await settle();
  await h.time.tick(89999); assert.equal(h.requests.length, 1);
  await h.time.tick(1); assert.equal(h.requests.length, 2); h.feed.close();
});

for (const status of [204, 401, 403, 429, 503]) {
  test(`HTTP ${status} never becomes a healthy stream and releases its body`, async () => {
    let cancelled = 0;
    const h = harness(() => ({ status, headers: { get: () => null }, body: { cancel: async () => { cancelled++; } } }));
    await settle(); assert.equal(h.feed.readyState, 0); assert.equal(cancelled, 1);
    assert.deepEqual(h.events.map(e => e.type), ['error']); h.feed.close();
  });
}

test('HTTP Retry-After remains a minimum across closing and replacing a feed', async () => {
  const time = clock(), gate = { until: 0 }, requests = [], good = body();
  const options = { now: time.now, wallNow: time.now, schedule: time.schedule, cancel: time.cancel, random: () => 0, gate,
    fetcher: async () => { requests.push(time.now()); return requests.length === 1
      ? { status: 429, headers: { get: () => '180' } } : good.response; } };
  const first = F.open('/events', options); await settle(); await time.tick(30000); first.close();
  const second = F.open('/events', options); await time.tick(149999); assert.deepEqual(requests, [0]);
  await time.tick(1); assert.deepEqual(requests, [0, 180000]); second.close(); assert.equal(time.pending, 0);
});

test('unrepresentable server delays do not overflow into an immediate retry', async () => {
  const h = harness(() => ({ status: 429, headers: { get: () => '9'.repeat(400) } }));
  await h.time.tick(600000); assert.equal(h.requests.length, 1); assert.equal(h.time.pending, 0);
  h.feed.close(); await settle(); assert.equal(h.feed.readyState, 2);
});

test('Retry-After accepts valid dates, not negative or fractional delta seconds', () => {
  assert.equal(F.retryAfter('Thu, 01 Jan 1970 00:00:03 GMT', 1000), 2000);
  assert.equal(F.retryAfter('Thu, 01 Jan 1970 00:00:00 GMT', 1000), 0);
  for (const v of ['', null, '-1', '+2', '1.5', 'invalid']) assert.equal(F.retryAfter(v, 0), 0);
});

test('a non-SSE success is not opened, and its unread body is cancelled', async () => {
  let cancelled = 0;
  const h = harness(() => ({ status: 200, headers: { get: () => 'text/html' }, body: { cancel: async () => { cancelled++; } } }));
  await settle(); assert.deepEqual(h.events.map(e => e.type), ['error']); assert.equal(cancelled, 1); h.feed.close();
});

test('view callback errors cannot interrupt delivery or replay a cursor', async () => {
  const stream = body(), h = harness(() => stream.response); let count = 0;
  h.feed.addEventListener('message', () => { throw new Error('render failed'); });
  h.feed.addEventListener('message', () => { count++; }); await settle();
  stream.send('id: 1\ndata: one\n\nid: 2\ndata: two\n\n'); await settle();
  assert.equal(count, 2); assert.equal(h.requests.length, 1); h.feed.close();
});

test('close inside an event listener fences the rest of the same network chunk', async () => {
  const stream = body(), h = harness(() => stream.response); await settle();
  h.feed.addEventListener('message', () => h.feed.close());
  stream.send('data: one\n\ndata: two\n\n'); await settle();
  assert.deepEqual(h.events.filter(e => e.type === 'message').map(e => e.data), ['one']);
  assert.equal(h.time.pending, 0); assert.equal(stream.cancelled, 1);
});

test('close before startup, during connection, or during retry cannot revive a feed', async () => {
  for (const stage of ['before', 'connecting', 'retry']) {
    const held = deferred(), h = harness(() => stage === 'retry' ? Promise.reject(new Error('offline')) : held.promise);
    if (stage !== 'before') await settle();
    h.feed.close(); h.feed.close(); await h.time.tick(100000);
    assert.equal(h.requests.length, stage === 'before' ? 0 : 1); assert.equal(h.feed.readyState, 2);
    assert.equal(h.time.pending, 0);
    held.resolve(body().response); await settle(); assert.equal(h.events.filter(e => e.type === 'open').length, 0);
  }
});

async function server(t, handler) {
  const s = http.createServer(handler);
  await new Promise((resolve, reject) => { s.once('error', reject); s.listen(0, '127.0.0.1', resolve); });
  t.after(() => new Promise(resolve => { s.close(resolve); s.closeAllConnections(); }));
  return `http://127.0.0.1:${s.address().port}`;
}

test('real HTTP reconnect sends the exact u64 cursor and receives split UTF-8 data', { timeout: 8000 }, async t => {
  const got = deferred(), requests = [];
  const base = await server(t, (req, res) => {
    requests.push({ method: req.method, path: req.url, id: req.headers['last-event-id'] });
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    if (requests.length === 1) res.end('id: 9007199254740993\nevent: zone.state\ndata: first\n\n');
    else {
      const bytes = Buffer.from('id: 9007199254740994\nevent: zone.state\ndata: Café 🎵\n\n');
      for (const byte of bytes) res.write(Buffer.from([byte]));
    }
  });
  const feed = F.open('/events', { fetcher: (url, init) => fetch(base + url, init), random: () => 0, gate: { until: 0 } });
  t.after(() => feed.close());
  feed.addEventListener('zone.state', e => { if (e.data === 'Café 🎵') got.resolve(e); });
  const e = await got.promise;
  assert.equal(e.lastEventId, '9007199254740994');
  assert.deepEqual(requests, [
    { method: 'GET', path: '/events', id: undefined }, { method: 'GET', path: '/events', id: '9007199254740993' },
  ]); feed.close();
});

test('real stalled socket is aborted at the body deadline and no late event escapes', { timeout: 8000 }, async t => {
  const received = deferred(), closed = deferred(), disconnected = deferred();
  let writes = 0;
  const base = await server(t, (req, res) => {
    writes++; res.on('close', closed.resolve);
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write('event: zone.state\ndata: baseline\n\n');
  });
  const time = clock();
  const feed = F.open('/events', { fetcher: (url, init) => fetch(base + url, init), now: time.now,
    schedule: time.schedule, cancel: time.cancel, random: () => 0, gate: { until: 0 } });
  t.after(() => feed.close());
  feed.addEventListener('zone.state', e => received.resolve(e)); feed.addEventListener('error', disconnected.resolve);
  await received.promise; await time.tick(F.IDLE_TIMEOUT); await disconnected.promise; await closed.promise;
  assert.equal(feed.readyState, 0); assert.equal(writes, 1); feed.close(); assert.equal(time.pending, 0);
});

test('real redirect never moves the event request to another path', { timeout: 8000 }, async t => {
  const refused = deferred(), paths = [];
  const base = await server(t, (req, res) => { paths.push(req.url); res.writeHead(307, { location: '/other' }); res.end(); });
  const feed = F.open('/events', { fetcher: (url, init) => fetch(base + url, init), gate: { until: 0 } });
  t.after(() => feed.close()); feed.addEventListener('error', refused.resolve);
  await refused.promise; feed.close(); assert.deepEqual(paths, ['/events']);
});
