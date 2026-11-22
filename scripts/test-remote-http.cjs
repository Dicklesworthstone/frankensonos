// node --test scripts/test-remote-http.cjs; real loopback sockets, no speakers/services.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const T = require('../crates/fsonos-api/src/remote/source-targets.js');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
function deferred() {
  let resolve;
  const promise = new Promise((yes) => { resolve = yes; });
  return { promise, resolve };
}
async function serve(t, handler) {
  const server = http.createServer(handler);
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  t.after(() => new Promise((resolve) => {
    server.close(resolve);
    server.closeAllConnections();
  }));
  return `http://127.0.0.1:${server.address().port}`;
}
function client(base, options = {}) {
  return vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\nObject.assign(ROUTES, SourceTargetsRemote.routes); call;', {
    AbortController, URLSearchParams, setTimeout, clearTimeout, SourceTargetsRemote: T,
    fetch: (where, init) => fetch(`${base}${where}`, init), ...options,
  });
}

test('the shipped client recovers from real HTTP 503s with only three GETs', { timeout: 5000 }, async (t) => {
  const requests = [];
  const base = await serve(t, (req, res) => {
    requests.push({ method: req.method, path: req.url });
    res.writeHead(requests.length < 3 ? 503 : 200, { 'content-type': 'application/json' });
    res.end(requests.length < 3 ? '{"code":"BUSY","detail":"Temporarily unavailable"}' : '[]');
  });
  const value = await client(base)('zones');
  assert.equal(value.length, 0);
  assert.deepEqual(requests, Array.from({ length: 3 }, () => ({ method: 'GET', path: '/zones' })));
});

test('a real lost write acknowledgement is never replayed; read-back observes its effect', { timeout: 5000 }, async (t) => {
  let writes = 0, reads = 0, volume = 20, body;
  const base = await serve(t, (req, res) => {
    if (req.method === 'GET') {
      reads++;
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ volume, mute: false }));
      return;
    }
    let text = '';
    req.on('data', (chunk) => { text += chunk; });
    req.on('end', () => {
      body = JSON.parse(text); volume = body.volume; writes++;
      req.socket.destroy(); // The effect exists; no acknowledgement reaches the client.
    });
  });
  const call = client(base);
  await assert.rejects(call('volume', { body: { zone: 'Den@S2', volume: 27 } }),
    (error) => error.code === 'REMOTE_OUTCOME_UNKNOWN' && error.uncertain && /not repeated/.test(error.hint));
  const observed = await call('state', { params: { room: 'Den@S2' } });
  assert.equal(observed.volume, 27);
  assert.deepEqual(body, { zone: 'Den@S2', volume: 27 });
  assert.equal(writes, 1); assert.equal(reads, 1);
});

test('a real response-body stall releases an uncertain write and closes its socket', { timeout: 5000 }, async (t) => {
  const reading = deferred(), closed = deferred();
  let writes = 0, expire, cleared = 0;
  const base = await serve(t, (req, res) => {
    writes++;
    req.resume();
    res.on('close', closed.resolve);
    res.writeHead(200, { 'content-type': 'application/json' });
    res.write('{"done":'); // Headers arrive, but the JSON acknowledgement never finishes.
  });
  const call = client(base, {
    // Advance just the shipped write deadline once the real body read starts.
    setTimeout(fn, ms) { assert.equal(ms, 120000); expire = fn; return 1; },
    clearTimeout() { cleared++; },
    fetch: async (where, init) => {
      const response = await fetch(`${base}${where}`, init);
      const text = response.text.bind(response);
      response.text = () => { reading.resolve(); return text(); };
      return response;
    },
  });
  const result = call('next', { body: { zone: 'Den@S2' } });
  const rejected = assert.rejects(result, (error) => error.code === 'REMOTE_OUTCOME_UNKNOWN' && error.uncertain);
  await reading.promise; expire(); await rejected; await closed.promise;
  assert.equal(writes, 1); assert.equal(cleared, 1);
});

test('a real HTTP redirect cannot relocate or replay a music command', { timeout: 5000 }, async (t) => {
  const paths = [];
  const base = await serve(t, (req, res) => {
    paths.push(req.url); req.resume();
    res.writeHead(307, { location: '/unexpected-mutation' }); res.end();
  });
  await assert.rejects(client(base)('next', { body: { zone: 'Den@S2' } }),
    (error) => error.code === 'REMOTE_OUTCOME_UNKNOWN' && error.uncertain);
  assert.deepEqual(paths, ['/next']);
});

test('a real source-control HTTP error preserves every playback target without retrying', { timeout: 5000 }, async (t) => {
  const groups = ['S1', 'S2', 'S3'].map(household => ({ household, coordinator_id: 'fixture-' + household, coordinator: 'Den' }));
  const batch = { complete: false, results: groups.map(g => ({ household: g.household, target: g.coordinator_id, zone: g.coordinator,
    result: { status: 'not_attempted' } })) };
  batch.results[0].result = { status: 'failed', error: { code: 'PLAYER_UNREACHABLE', detail: 'First speaker is offline.', retryable: true } };
  batch.results[1].result = { status: 'failed', error: { code: 'OUTCOME_UNCERTAIN', detail: 'Playback may already have started.', retryable: false } };
  let writes = 0;
  const base = await serve(t, (req, res) => {
    writes++; assert.equal(req.method, 'POST'); assert.equal(req.url, '/sources/control'); req.resume();
    res.writeHead(503, { 'content-type': 'application/json', 'retry-after': '1' });
    res.end(JSON.stringify({ code: 'PLAYER_UNREACHABLE', detail: 'First speaker is offline.', retryable: false, batch }));
  });
  await assert.rejects(client(base)('sourceControl', { body: { zone: 'everywhere', mode: 'play' } }), error => {
    assert.deepEqual(JSON.parse(JSON.stringify(error.batch)), batch);
    const receipt = T.failure(error, { mode: 'play', groups }, P);
    assert.equal(receipt.status, 'uncertain'); assert.equal(receipt.complete, false);
    assert.deepEqual(receipt.targets.map(row => row.status), ['failed', 'uncertain', 'not_attempted']);
    for (const label of ['Den@S1', 'Den@S2', 'Den@S3']) assert.ok(receipt.detail.includes(label));
    assert.match(receipt.detail, /Do not repeat the whole batch/);
    return true;
  });
  assert.equal(writes, 1);
});
