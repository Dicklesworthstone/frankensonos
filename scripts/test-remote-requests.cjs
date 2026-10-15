// node --test scripts/test-remote-requests.cjs; real shipped client, fake time/network.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(process.env.REMOTE_SOURCE
  || path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
const settle = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
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
    schedule(fn, delay) { timers.set(++serial, { fn, at: now + delay }); return serial; },
    cancel(id) { timers.delete(id); },
    get pending() { return timers.size; },
    async tick(ms) {
      const end = now + ms;
      await settle();
      for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end)
          .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}
function response(status = 200, data = { done: 'Done', changed: true }, options = {}) {
  return { status, ok: status >= 200 && status < 300,
    headers: { get: (key) => options.headers?.[key] ?? null },
    text: () => options.body ? options.body() : Promise.resolve(options.raw ?? JSON.stringify(data)) };
}
function observe(promise) {
  const result = { settled: false };
  result.done = promise.then((value) => { result.value = value; }, (error) => { result.error = error; })
    .then(() => { result.settled = true; });
  return result;
}
function harness(fetcher) {
  const time = clock(), calls = [];
  const fetch = (where, init) => { calls.push({ where, init, at: time.now() }); return fetcher(where, init, calls.length); };
  const sandbox = { setTimeout: time.schedule, clearTimeout: time.cancel, AbortController,
    URLSearchParams, fetch };
  const exports = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\n({ RequestRemote, call, ROUTES });', sandbox);
  const request = exports.RequestRemote.create({ fetcher: fetch,
    schedule: time.schedule, cancel: time.cancel, now: time.now, random: () => 0 });
  return { time, calls, request, ...exports };
}

test('the public call still uses the registered path, JSON body and same-origin credentials', async () => {
  const h = harness(async () => response());
  const value = await h.call('queuePlay', { body: { zone: 'Den@Home', position: 4 } });
  assert.equal(value.done, 'Done');
  assert.equal(h.calls.length, 1);
  const { where, init } = h.calls[0];
  assert.equal(where, '/queue/play');
  assert.equal(init.method, 'POST');
  assert.deepEqual(JSON.parse(init.body), { zone: 'Den@Home', position: 4 });
  assert.equal(init.headers['Content-Type'], 'application/json');
  assert.equal(init.headers.Accept, 'application/json');
  assert.equal(init.cache, 'no-store');
  assert.equal(init.credentials, 'same-origin');
  assert.equal(init.redirect, 'error', 'a redirect must not send a command somewhere else');
  assert.equal(h.time.pending, 0);
});

test('registered GET parameters stay household-qualified and encoded', async () => {
  const h = harness(async () => response(200, []));
  await h.call('queue', { params: { room: 'Den / A@House' }, query: { offset: 25, limit: 25 } });
  assert.equal(h.calls[0].where, '/zones/Den%20%2F%20A%40House/queue?offset=25&limit=25');
  assert.equal(h.calls[0].init.body, undefined);
});

for (const stage of ['connection', 'body']) {
  test(`a stuck write ${stage} settles even when fetch ignores abort, and is sent only once`, async () => {
    const waiting = deferred();
    const h = harness(async () => stage === 'connection' ? waiting.promise
      : response(200, null, { body: () => waiting.promise }));
    const result = observe(h.call('next', { body: { zone: 'Den' } }));
    await h.time.tick(119999); assert.equal(result.settled, false);
    await h.time.tick(1); assert.equal(result.settled, true);
    assert.equal(result.error.code, 'REMOTE_OUTCOME_UNKNOWN');
    assert.equal(result.error.uncertain, true);
    assert.match(result.error.hint, /may still finish/);
    assert.match(result.error.hint, /not repeated/);
    assert.equal(h.calls[0].init.signal.aborted, true);
    assert.equal(h.calls.length, 1);
    assert.equal(h.time.pending, 0);
    waiting.resolve(stage === 'connection' ? response() : '{"done":"Late"}');
    await h.time.tick(600000);
    assert.equal(result.value, undefined, 'a late acknowledgement cannot change the settled outcome');
    assert.equal(h.calls.length, 1);
  });

  test(`a stuck read ${stage} cannot hold live recovery past its whole-request deadline`, async () => {
    const waiting = deferred();
    const h = harness(async () => stage === 'connection' ? waiting.promise
      : response(200, null, { body: () => waiting.promise }));
    const result = observe(h.request('/zones', { method: 'GET' }));
    await h.time.tick(15000);
    assert.equal(result.settled, true);
    assert.equal(result.error.code, 'REMOTE_READ_TIMEOUT');
    assert.equal(result.error.uncertain, false);
    assert.equal(h.calls.length, 1, 'a stalled attempt does not create overlapping retries');
    assert.equal(h.time.pending, 0);
    waiting.reject(new Error('late failure')); await settle();
  });
}

test('normal success cancels its timer and never aborts a completed request later', async () => {
  const h = harness(async () => response());
  await h.request('/pause', { method: 'POST' });
  await h.time.tick(1200000);
  assert.equal(h.time.pending, 0); assert.equal(h.calls[0].init.signal.aborted, false);
});

test('network failure is retried for reads only with bounded exponential backoff', async () => {
  const h = harness(async (_, __, n) => { if (n < 3) throw new TypeError('offline'); return response(200, ['room']); });
  const result = observe(h.request('/zones', { method: 'GET' }));
  await h.time.tick(249); assert.equal(h.calls.length, 1);
  await h.time.tick(1); assert.equal(h.calls.length, 2);
  await h.time.tick(499); assert.equal(h.calls.length, 2);
  await h.time.tick(1); await result.done;
  assert.equal(result.value[0], 'room');
  assert.deepEqual(h.calls.map((call) => call.at), [0, 250, 750]);
  assert.equal(h.time.pending, 0);
});

test('a permanent outage stops at three read attempts', async () => {
  const h = harness(async () => { throw new TypeError('offline'); });
  const result = observe(h.request('/zones', { method: 'GET' }));
  await h.time.tick(15000);
  assert.equal(result.error.code, 'REMOTE_NETWORK_ERROR');
  assert.equal(h.calls.length, 3); assert.equal(h.time.pending, 0);
});

test('a write rejected by the network is uncertain and never retried', async () => {
  const h = harness(async () => { throw new TypeError('socket lost after send'); });
  const result = observe(h.request('/queue', { method: 'POST', body: '{}' }));
  await h.time.tick(1200000);
  assert.equal(result.error.uncertain, true); assert.equal(h.calls.length, 1);
  assert.equal(h.time.pending, 0);
});

for (const status of [408, 429, 502, 503, 504]) {
  test(`HTTP ${status} permits read recovery but never a write replay`, async () => {
    const h = harness(async (_, __, n) => n === 1 ? response(status, { code: 'BUSY', detail: 'Try later' }) : response());
    const result = observe(h.request('/zones', { method: 'GET' }));
    await h.time.tick(250); await result.done;
    assert.equal(result.value.done, 'Done'); assert.equal(h.calls.length, 2);
    const write = harness(async () => response(status, { code: 'BUSY', detail: 'Try later' }));
    const refused = observe(write.request('/next', { method: 'POST' }));
    await write.time.tick(1200000);
    assert.equal(refused.error.status, status); assert.equal(write.calls.length, 1);
    assert.equal(refused.error.uncertain, status !== 429);
  });
}

for (const status of [400, 401, 403, 404, 409, 422, 500, 501]) {
  test(`HTTP ${status} preserves the API error without read retries`, async () => {
    const h = harness(async () => response(status, { code: 'POLICY_DENIED', detail: 'Not allowed', hint: 'Ask the owner' }));
    const result = observe(h.request('/zones', { method: 'GET' }));
    await h.time.tick(15000);
    assert.equal(result.error.code, 'POLICY_DENIED'); assert.equal(result.error.message, 'Not allowed');
    assert.equal(result.error.hint, 'Ask the owner'); assert.equal(result.error.status, status);
    assert.equal(h.calls.length, 1); assert.equal(h.time.pending, 0);
  });
}

test('an API code cannot disguise a permanent HTTP error as a network retry', async () => {
  const h = harness(async () => response(403, { code: 'REMOTE_NETWORK_ERROR' }));
  const result = observe(h.request('/zones', { method: 'GET' }));
  await h.time.tick(15000); assert.equal(h.calls.length, 1); assert.equal(result.error.status, 403);
});

test('Retry-After delta-seconds is honored without shortening it to the backoff', async () => {
  const h = harness(async (_, __, n) => n === 1
    ? response(429, {}, { headers: { 'retry-after': '2' } }) : response());
  const result = observe(h.request('/zones', { method: 'GET' }));
  await h.time.tick(1999); assert.equal(h.calls.length, 1);
  await h.time.tick(1); await result.done;
  assert.equal(result.value.done, 'Done'); assert.equal(h.calls[1].at, 2000);
});

test('Retry-After beyond the total deadline is not capped into an early request', async () => {
  for (const value of ['15', '3600', '9'.repeat(400)]) {
    const h = harness(async () => response(429, { detail: 'Rate limited' }, { headers: { 'retry-after': value } }));
    const result = observe(h.request('/zones', { method: 'GET' }));
    await h.time.tick(15000);
    assert.equal(result.error.status, 429); assert.equal(h.calls.length, 1); assert.equal(h.time.pending, 0);
  }
});

test('HTTP date retry hints use the server minimum; malformed and signed durations are ignored', () => {
  const h = harness(async () => response());
  assert.equal(h.RequestRemote.retryAfter('Thu, 01 Jan 1970 00:00:03 GMT', 1000), 2000);
  assert.equal(h.RequestRemote.retryAfter('Thu, 01 Jan 1970 00:00:00 GMT', 1000), 0);
  for (const text of ['', '-1', '1.5', '+2', 'not a date']) assert.equal(h.RequestRemote.retryAfter(text, 0), null);
});

test('retries share the original deadline instead of each buying another fifteen seconds', async () => {
  const waiting = deferred();
  const h = harness(async (_, __, n) => { if (n === 1) throw new TypeError('offline'); return waiting.promise; });
  const result = observe(h.request('/zones', { method: 'GET' }));
  await h.time.tick(15000);
  assert.equal(result.error.code, 'REMOTE_READ_TIMEOUT'); assert.equal(h.calls.length, 2);
  assert.equal(h.time.pending, 0);
  waiting.resolve(response()); await settle(); assert.equal(h.calls.length, 2);
});

for (const raw of ['', '<html>Proxy error</html>', '{', 'undefined']) {
  test(`unreadable success ${JSON.stringify(raw)} cannot silently acknowledge a write`, async () => {
    for (const method of ['GET', 'POST']) {
      const h = harness(async () => response(200, null, { raw }));
      const result = observe(h.request('/queue/play', { method }));
      await h.time.tick(15000);
      assert.equal(result.error.code, method === 'GET' ? 'REMOTE_INVALID_RESPONSE' : 'REMOTE_OUTCOME_UNKNOWN');
      assert.equal(result.error.uncertain, method === 'POST'); assert.equal(h.calls.length, 1);
    }
  });
}

test('valid JSON null and a no-content response preserve optional-result endpoints', async () => {
  for (const reply of [response(200, null), response(204, null, { raw: '' })]) {
    const h = harness(async () => reply);
    assert.equal(await h.request('/dj/active', { method: 'GET' }), null);
    assert.equal(h.time.pending, 0);
  }
});

test('long-running actions get finite route-aware deadlines, not the short read timeout', () => {
  const { RequestRemote: r } = harness(async () => response());
  assert.equal(r.timeoutFor('GET', '/announce'), 15000);
  assert.equal(r.timeoutFor('POST', '/pause/fade'), 120000);
  for (const where of ['/dj/start', '/dj/steer', '/queue', '/scenes/Quiet%20night/apply']) {
    assert.equal(r.timeoutFor('POST', where), 600000);
  }
  assert.equal(r.timeoutFor('POST', '/announce'), 1200000);
  assert.equal(r.timeoutFor('POST', '/announcements/not-a-real-path'), 120000);
});
