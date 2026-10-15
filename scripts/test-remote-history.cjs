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

const U = H.undo;
const undone = (target = row(), extra = {}) => ({ undone: target.id, intent: target.intent,
  summary: `undid #${target.id}`, ...extra });
function undoHarness(options = {}) {
  const calls = []; let preview = row(), result = undone(), settlements = 0;
  const control = U.model({ call: async (name, options) => {
    calls.push({ name, options });
    const value = name === 'undoPreview' ? preview : result;
    return typeof value === 'function' ? value() : structuredClone(value);
  }, settled: () => { ++settlements; }, ...options });
  return { control, calls, preview(value) { preview = value; }, result(value) { result = value; },
    writes: () => calls.filter((c) => c.name === 'undoChecked'), settled: () => settlements };
}

test('undo preview accepts only an eligible caller action or explicit null', () => {
  assert.equal(U.previewOf(null), null);
  assert.equal(U.previewOf(row()).id, 7);
  for (const value of [undefined, [], {}, row({ undoable: false }),
    row({ client: '' }), row({ intent: '' }), row({ undoable: false, undo_of: 6 })]) {
    assert.throws(() => U.previewOf(value));
  }
});

test('undo result is tied to the reviewed ID and intent, with literal partial failures', () => {
  const full = U.resultOf(undone(), row());
  assert.deepEqual(full.failures, []); assert.ok(Object.isFrozen(full));
  const partial = U.resultOf(undone(row(), { failures: ['SYNTHETIC Position: queue changed', '<img>'] }), row());
  assert.equal(partial.failures.length, 2); assert.ok(Object.isFrozen(partial.failures));
  for (const value of [null, {}, undone(row(), { undone: 8 }), undone(row(), { intent: 'another intent' }),
    undone(row(), { summary: '' }), undone(row(), { failures: null }), undone(row(), { failures: [false] }),
    undone(row(), { failures: [''] }), undone(row(), { failures: Array(2001).fill('failure') })]) {
    assert.throws(() => U.resultOf(value, row()));
  }
});

test('reading, reviewing and cancelling undo do not change anything', async () => {
  const h = undoHarness();
  assert.equal(h.control.review(), false);
  await h.control.refresh(); assert.equal(h.control.review(), true);
  h.control.cancel(); assert.equal(h.control.state.pending, null);
  assert.equal(h.writes().length, 0);
  h.preview(null); await h.control.refresh(); assert.equal(h.control.review(), false);
});

test('undo confirmation sends exactly one expected ID and consumes the review', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  const pending = h.control.state.pending;
  assert.equal(await h.control.confirm(pending), true);
  assert.deepEqual(h.writes(), [{ name: 'undoChecked', options: { body: { expected_id: 7 } } }]);
  assert.equal(h.control.state.result.undone, 7);
  assert.equal(h.control.state.loaded, false); assert.equal(h.control.state.target, null);
  assert.equal(h.control.review(), false); assert.equal(await h.control.confirm(pending), false);
  assert.equal(h.settled(), 1);
});

test('undo reports partial restoration instead of flattening it into success', async () => {
  const h = undoHarness(); h.result(undone(row(), { failures: ['Queue position not restored'] }));
  await h.control.refresh(); h.control.review();
  assert.equal(await h.control.confirm(h.control.state.pending), true);
  assert.match(h.control.state.message, /incomplete/);
  assert.deepEqual(h.control.state.result.failures, ['Queue position not restored']);
});

test('undo double clicks serialize both preflight and submission', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  const pending = h.control.state.pending, reply = delay(); h.result(() => reply.promise);
  const first = h.control.confirm(pending);
  assert.equal(await h.control.confirm(pending), false);
  await new Promise(setImmediate);
  assert.equal(h.writes().length, 1);
  assert.equal(h.control.review(), false);
  const count = h.calls.length; assert.equal(await h.control.refresh(), false); assert.equal(h.calls.length, count);
  reply.resolve(undone()); assert.equal(await first, true);
});

test('cancelled and superseded undo review objects cannot dispatch', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  const first = h.control.state.pending;
  h.control.cancel(); assert.equal(await h.control.confirm(first), false);
  h.control.review(); const second = h.control.state.pending;
  await h.control.refresh(); assert.equal(await h.control.confirm(second), false);
  assert.equal(h.writes().length, 0);
});

for (const latest of [null, row({ id: 8 }), row({ client: 'another-client' }), row({ result: 'different result' })]) {
  test(`undo preflight rejects a changed observation ${JSON.stringify(latest)}`, async () => {
    const h = undoHarness(); await h.control.refresh(); h.control.review(); h.preview(latest);
    assert.equal(await h.control.confirm(h.control.state.pending), false);
    assert.equal(h.writes().length, 0); assert.equal(h.control.state.loaded, false);
    assert.match(h.control.state.error, /changed/);
  });
}

test('unreadable undo preflight and unavailable endpoints never fall back to legacy undo', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review(); h.preview({});
  assert.equal(await h.control.confirm(h.control.state.pending), false);
  h.preview(() => { throw Object.assign(new Error('Endpoint not found'), { code: 'NOT_FOUND' }); });
  assert.equal(await h.control.refresh(), false);
  assert.equal(h.control.review(), false);
  assert.ok(h.calls.every((c) => c.name === 'undoPreview'));
});

test('a lost undo reply is uncertain, never replayed, and refresh remains read-only', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  h.result(() => { throw new Error('Connection lost'); });
  assert.equal(await h.control.confirm(h.control.state.pending), false);
  assert.match(h.control.state.error, /uncertain.*No request was repeated/);
  assert.equal(h.control.review(), false); assert.equal(h.writes().length, 1);
  h.preview(null); await h.control.refresh();
  assert.equal(h.writes().length, 1); assert.equal(h.settled(), 1);
});

test('a contradictory undo acknowledgement is not displayed as restoration', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  h.result(undone(row(), { undone: 8 }));
  assert.equal(await h.control.confirm(h.control.state.pending), false);
  assert.equal(h.control.state.result, null); assert.match(h.control.state.error, /uncertain/);
  assert.equal(h.writes().length, 1);
});

for (const end of ['close', 'dispose']) {
  test(`${end} during undo preflight invalidates the review before dispatch`, async () => {
    const h = undoHarness(); await h.control.refresh(); h.control.review();
    const wait = delay(); h.preview(() => wait.promise);
    const task = h.control.confirm(h.control.state.pending);
    h.control[end](); wait.resolve(row());
    assert.equal(await task, false); assert.equal(h.writes().length, 0);
  });
}

test('closing a sent undo does not replay it on reopening', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  const wait = delay(); h.result(() => wait.promise);
  const task = h.control.confirm(h.control.state.pending); await new Promise(setImmediate);
  h.control.close(); assert.equal(await h.control.refresh(), false);
  wait.resolve(undone()); assert.equal(await task, true);
  assert.equal(h.writes().length, 1); assert.equal(h.control.state.loaded, false);
});

test('superseded undo preview reads cannot overwrite the newer result', async () => {
  const h = undoHarness(); const wait = delay(); h.preview(() => wait.promise);
  const first = h.control.refresh(); h.preview(row({ id: 8 })); await h.control.refresh();
  wait.resolve(row()); assert.equal(await first, false); assert.equal(h.control.state.target.id, 8);
});

test('failed undo-preview refresh clears the old actionable state', async () => {
  const h = undoHarness(); await h.control.refresh(); h.control.review();
  h.preview(() => { throw new Error('offline'); });
  assert.equal(await h.control.refresh(), false);
  assert.equal(h.control.state.target, null); assert.equal(h.control.state.pending, null);
  assert.equal(h.control.review(), false);
});

test('render or post-write notification errors never turn an undo into a retry', async () => {
  const h = undoHarness({ changed: () => { throw new Error('render'); }, settled: () => { throw new Error('notify'); } });
  await h.control.refresh(); h.control.review();
  assert.equal(await h.control.confirm(h.control.state.pending), true);
  assert.equal(h.writes().length, 1); assert.equal(h.control.state.result.undone, 7);
});

test('localhost checked undo contract rejects a target changed after client preflight', async (t) => {
  const requests = []; let previewReads = 0, eligible = 7;
  const server = http.createServer((req, res) => {
    let body = ''; req.on('data', (chunk) => { body += chunk; }); req.on('end', () => {
      requests.push({ method: req.method, path: req.url, body });
      res.setHeader('Content-Type', 'application/json');
      if (req.method === 'GET' && req.url === '/undo/preview') {
        res.end(JSON.stringify(row()));
        if (++previewReads === 2) eligible = 8;
      } else if (req.method === 'POST' && req.url === '/undo/checked') {
        assert.equal(body, '{"expected_id":7}');
        const expected = JSON.parse(body).expected_id;
        if (expected !== eligible) { res.statusCode = 422; res.end(JSON.stringify({ detail: 'Reviewed target changed', code: 'INVALID_ARGUMENT' })); }
        else res.end(JSON.stringify(undone()));
      } else { res.statusCode = 404; res.end('{}'); }
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const control = U.model({ call: async (name, { body } = {}) => {
    const [method, route] = U.routes[name];
    const response = await fetch(origin + route, { method, headers: { 'Content-Type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined });
    const value = await response.json();
    if (!response.ok) throw Object.assign(new Error(value.detail), { code: value.code });
    return value;
  } });
  await control.refresh(); control.review();
  assert.equal(await control.confirm(control.state.pending), false);
  assert.deepEqual(requests.map((r) => [r.method, r.path]), [
    ['GET', '/undo/preview'], ['GET', '/undo/preview'], ['POST', '/undo/checked']]);
  assert.match(control.state.error, /target changed/); assert.equal(control.state.result, null);
});
