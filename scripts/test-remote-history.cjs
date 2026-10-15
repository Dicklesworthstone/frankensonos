'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const H = require('../crates/fsonos-api/src/remote/history.js');
const row = (extra = {}) => ({ id: 7, at: 1792015200, client: 'sample-client', surface: 'http',
  intent: 'set_volume: Kitchen@SYNTHETIC', decision: 'clamp: room cap',
  result: 'Kitchen volume is 30', undoable: true, ...extra });
const delay = () => { let resolve, reject; const promise = new Promise((r, j) => { resolve = r; reject = j; }); return { promise, resolve, reject }; };
function harness() {
  const calls = []; let answer = [row()];
  const control = H.model({ call: async (name, options) => {
    calls.push({ name, options }); return typeof answer === 'function' ? answer() : structuredClone(answer);
  } });
  return { control, calls, answer(value) { answer = value; } };
}

test('accepts and freezes normal, denied and undo log DTOs', () => {
  for (const value of [row(), row({ decision: 'deny: not allowed', undoable: false }),
    row({ id: 8, undoable: false, undo_of: 7 })]) {
    const result = H.actionOf(value);
    assert.equal(result.id, value.id); assert.ok(Object.isFrozen(result));
  }
});

test('rejects malformed IDs, timestamps, fields and contradictory undo markers', () => {
  for (const bad of [null, {}, ...[0, -1, 1.5, '7', Number.MAX_SAFE_INTEGER + 1].map((id) => row({ id })),
    ...['today', Infinity, 8640000000001].map((at) => row({ at })),
    row({ client: null }), row({ surface: 9 }), row({ intent: 'x'.repeat(65537) }),
    row({ result: null }), row({ decision: [] }), row({ undoable: 'true' }),
    row({ undo_of: 1 }), row({ undoable: false, undo_of: 7 }), row({ undoable: false, undo_of: '6' })]) {
    assert.throws(() => H.actionOf(bad));
  }
});

test('refuses duplicate or oversized lists and preserves server ordering', () => {
  for (const bad of [{}, null, [row(), row()], [row(), row({ id: 8 })]]) {
    assert.throws(() => H.actionsOf(bad, 1));
  }
  const values = [row({ id: 9, at: 100 }), row({ id: 8, at: 200 })];
  assert.deepEqual(H.actionsOf(values, 20).map((r) => r.id), [9, 8]);
});

test('search matches Unicode, client, intent, decision, result and ID without mutations', () => {
  const values = H.actionsOf([row({ intent: 'ÉTUDE — <img onerror=bad()>', client: 'A & B' }), row({ id: 10, decision: 'allow' })], 20);
  for (const q of ['étude', 'a & b', 'clamp:', '7', '<img']) assert.deepEqual(H.visible(values, q).map((r) => r.id), [7]);
  assert.equal(H.visible(values, 'volume is 30').length, 2);
  assert.equal(H.visible(values, 'missing').length, 0);
  assert.equal(H.visible(values, '  ').length, 2);
});

test('default reads use exactly the bounded GET contract', async () => {
  const h = harness(); assert.equal(await h.control.refresh(), true);
  assert.deepEqual(h.calls, [{ name: 'historyRead', options: { query: { limit: 20 } } }]);
  assert.equal(h.control.state.loaded, true); assert.equal(h.control.state.stale, false);
  h.control.search('Kitchen'); assert.equal(h.calls.length, 1);
});

test('exact-client filters use server query, clear prior rows and preserve literal names', async () => {
  const h = harness(); await h.control.refresh();
  assert.equal(h.control.filters('  A & B / Étude  ', 100), true);
  assert.deepEqual(h.control.state.rows, []);
  h.answer([row({ client: 'A & B / Étude' })]);
  assert.equal(await h.control.refresh(), true);
  assert.deepEqual(h.calls[1].options.query, { limit: 100, client: 'A & B / Étude' });
});

test('invalid filter edits are inert and a mismatched client response is refused', async () => {
  const h = harness();
  for (const args of [[null, 20], ['A', 0], ['A', 1000], ['A', '20'], ['x'.repeat(2049), 20]]) {
    assert.equal(h.control.filters(...args), false);
  }
  assert.equal(h.control.state.client, '');
  h.control.filters('other', 20);
  assert.equal(await h.control.refresh(), false);
  assert.equal(h.control.state.loaded, false); assert.match(h.control.state.error, /selected client/);
});

test('failed refresh retains a visibly stale snapshot and API error hint', async () => {
  const h = harness(); await h.control.refresh();
  h.answer(() => { throw Object.assign(new Error('offline'), { hint: 'Check the daemon.' }); });
  assert.equal(await h.control.refresh(), false);
  assert.equal(h.control.state.loaded, true); assert.equal(h.control.state.stale, true);
  assert.equal(h.control.state.rows[0].id, 7);
  assert.match(h.control.state.error, /offline Check the daemon/);
  h.answer([]); assert.equal(await h.control.refresh(), true);
  assert.equal(h.control.state.stale, false); assert.equal(h.control.state.rows.length, 0);
});

test('superseded reads cannot replace a newer observation', async () => {
  const h = harness(); const old = delay(); h.answer(() => old.promise);
  const first = h.control.refresh(); h.answer([row({ id: 8 })]); await h.control.refresh();
  old.resolve([row()]); assert.equal(await first, false);
  assert.equal(h.control.state.rows[0].id, 8);
});

for (const end of ['close', 'dispose']) {
  test(`${end} invalidates unfinished reads`, async () => {
    const h = harness(); const wait = delay(); h.answer(() => wait.promise);
    const read = h.control.refresh(); h.control[end](); wait.resolve([row()]);
    assert.equal(await read, false); assert.deepEqual(h.control.state.rows, []);
  });
}

test('filter edits invalidate in-flight reads even before the next refresh', async () => {
  const h = harness(); const wait = delay(); h.answer(() => wait.promise);
  const read = h.control.refresh(); h.control.filters('second-client', 500);
  wait.resolve([row()]); assert.equal(await read, false);
  assert.deepEqual(h.control.state.rows, []); assert.equal(h.control.state.loading, false);
});

test('all read-only model operations leave the mutation surface untouched', async () => {
  const h = harness(); await h.control.refresh(); h.control.search('clamp'); h.control.close();
  await h.control.refresh(); h.control.filters('', 100); await h.control.refresh();
  h.control.dispose(); await h.control.refresh();
  assert.ok(h.calls.every((c) => H.routes[c.name][0] === 'GET'));
});

test('real localhost request preserves exact encoded client query and returns the DTO', async (t) => {
  const requests = []; const client = 'Example & Étude / QA';
  const server = http.createServer((req, res) => {
    requests.push({ method: req.method, url: req.url });
    const url = new URL(req.url, 'http://fixture.invalid');
    assert.equal(url.pathname, '/actions'); assert.equal(url.searchParams.get('client'), client);
    assert.equal(url.searchParams.get('limit'), '100');
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify([row({ client })]));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const control = H.model({ call: async (name, { query }) => {
    const [method, route] = H.routes[name]; const response = await fetch(`${origin}${route}?${new URLSearchParams(query)}`, { method });
    assert.ok(response.ok); return response.json();
  } });
  control.filters(client, 100); assert.equal(await control.refresh(), true);
  assert.equal(requests.length, 1); assert.equal(requests[0].method, 'GET');
  assert.equal(control.state.rows[0].client, client);
});

test('the served bundle includes both history assets and the secret-free scan', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/http/remote.rs'), 'utf8');
  for (const kind of ['SCRIPT', 'STYLE']) assert.ok(source.includes(`{HISTORY_${kind}}`));
  for (const ext of ['js', 'css']) {
    assert.ok(source.includes(`("history.${ext}", HISTORY_`));
    const asset = fs.readFileSync(path.join(__dirname, `../crates/fsonos-api/src/remote/history.${ext}`), 'utf8');
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.equal(asset.toLowerCase().includes(secret), false, secret);
    }
  }
});
