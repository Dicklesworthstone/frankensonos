'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const R = require('../crates/fsonos-api/src/remote/read-requests.js');
const read = (signal) => ({ method: 'GET', headers: { Accept: 'application/json' }, cache: 'no-store', ...(signal ? { signal } : {}) });
const write = (method = 'POST') => ({ method, headers: { 'Content-Type': 'application/json' }, body: '{}' });
function gate() { let resolve, reject; const promise = new Promise((ok, no) => { resolve = ok; reject = no; }); return { promise, resolve, reject }; }
const tick = () => new Promise(setImmediate);
function fixture(options) {
  const calls = [];
  const request = R.create((where, init) => { const wait = gate(); calls.push({ where, init, ...wait }); return wait.promise; }, options);
  return { calls, request };
}

test('50 concurrent household readers perform one exchange, then a fresh read performs another', async () => {
  const f = fixture(); const tasks = Array.from({ length: 50 }, () => f.request('/zones', read()));
  await tick(); assert.equal(f.calls.length, 1);
  f.calls[0].resolve([{ household: 'SYNTHETIC', members: ['Room'] }]);
  const replies = await Promise.all(tasks);
  replies[0][0].members.push('mutated'); assert.deepEqual(replies[1][0].members, ['Room']);
  const next = f.request('/zones', read()); await tick(); assert.equal(f.calls.length, 2);
  f.calls[1].resolve([]); assert.deepEqual(await next, []);
});

test('different household paths, queries and revisions never merge', async () => {
  const f = fixture(); const paths = ['/zones/Room%40S1/state', '/zones/Room%40S2/state',
    '/zones/Room%40S1/queue?offset=0&revision=a', '/zones/Room%40S1/queue?offset=0&revision=b',
    '/zones/Room%40S1/queue?offset=50&revision=a', '/favorites?zone=A%2BB', '/favorites?zone=A+B'];
  const tasks = paths.map((p) => f.request(p, read())); await tick();
  assert.deepEqual(f.calls.map((c) => c.where), paths);
  f.calls.forEach((c, i) => c.resolve(i)); assert.deepEqual(await Promise.all(tasks), paths.map((_, i) => i));
});

test('JSON scalar/null results retain their original shapes', async () => {
  for (const value of [null, false, 0, 'literal', [1, 2]]) {
    const f = fixture(); const a = f.request('/read', read()), b = f.request('/read', read());
    await tick(); f.calls[0].resolve(value); assert.deepEqual(await a, value); assert.deepEqual(await b, value);
  }
});

test('a failure fans out once and does not poison the next observation', async () => {
  const f = fixture(); const error = Object.assign(new Error('offline'), { code: 'REMOTE_NETWORK_ERROR' });
  const result = Promise.allSettled([f.request('/zones', read()), f.request('/zones', read())]);
  await tick(); f.calls[0].reject(error);
  for (const r of await result) { assert.equal(r.status, 'rejected'); assert.equal(r.reason.code, error.code); }
  const next = f.request('/zones', read()); await tick(); assert.equal(f.calls.length, 2);
  f.calls[1].resolve([]); assert.deepEqual(await next, []);
});

test('a synchronous transport exception releases the entry', async () => {
  let calls = 0; const request = R.create(() => { ++calls; throw new Error('synchronous'); });
  await Promise.all([assert.rejects(request('/zones', read()), /synchronous/), assert.rejects(request('/zones', read()), /synchronous/)]);
  assert.equal(calls, 1); await assert.rejects(request('/zones', read()), /synchronous/); assert.equal(calls, 2);
});

for (const method of ['POST', 'PUT', 'DELETE', 'PATCH']) test(`${method} is never coalesced or repeated`, async () => {
  const f = fixture(); const tasks = [f.request('/operation', write(method)), f.request('/operation', write(method))];
  assert.equal(f.calls.length, 2); f.calls.forEach((c) => c.resolve({ done: method })); await Promise.all(tasks);
});

test('command start/end barriers prevent reuse of pre-command and during-command reads', async () => {
  const f = fixture(); const before = f.request('/zones', read()); await tick();
  const command = f.request('/pause', write());
  const during = [f.request('/zones', read()), f.request('/zones', read())];
  assert.equal(f.calls.length, 4); f.calls[1].resolve({ done: 'paused' }); await command;
  const after = [f.request('/zones', read()), f.request('/zones', read())]; await tick();
  assert.equal(f.calls.length, 5);
  f.calls[0].resolve('before'); f.calls[2].resolve('during1'); f.calls[3].resolve('during2'); f.calls[4].resolve('after');
  assert.equal(await before, 'before'); assert.deepEqual(await Promise.all(during), ['during1', 'during2']);
  assert.deepEqual(await Promise.all(after), ['after', 'after']);
});

test('failure and overlapping commands retain barriers until all commands settle', async () => {
  const f = fixture(); const one = f.request('/one', write()), two = f.request('/two', write());
  const refused = assert.rejects(one, /uncertain/); f.calls[0].reject(new Error('uncertain')); await refused;
  const middle = [f.request('/zones', read()), f.request('/zones', read())];
  assert.equal(f.calls.length, 4); f.calls[1].resolve({ done: 'two' }); await two;
  const after = [f.request('/zones', read()), f.request('/zones', read())]; await tick(); assert.equal(f.calls.length, 5);
  f.calls.slice(2).forEach((c) => c.resolve([])); await Promise.all([...middle, ...after]);
});

test('an old read settling cannot remove a post-command shared observation', async () => {
  const f = fixture(); const old = f.request('/zones', read()); await tick();
  const command = f.request('/pause', write()); f.calls[1].resolve({}); await command;
  const fresh = f.request('/zones', read()); f.calls[0].resolve('old');
  const join = f.request('/zones', read()); await old; await tick(); assert.equal(f.calls.length, 3);
  f.calls[2].resolve('fresh'); assert.deepEqual(await Promise.all([fresh, join]), ['fresh', 'fresh']);
});

test('one cancelled reader cannot abort the other reader', async () => {
  const f = fixture(), controller = new AbortController();
  const first = f.request('/zones', read(controller.signal)), second = f.request('/zones', read());
  const rejected = assert.rejects(first, { name: 'AbortError' }); await tick(); controller.abort(); await rejected;
  assert.equal(f.calls[0].init.signal.aborted, false); f.calls[0].resolve({ good: true }); assert.deepEqual(await second, { good: true });
});

test('last cancelled reader aborts the underlying read and permits a fresh exchange', async () => {
  const f = fixture(), a = new AbortController(), b = new AbortController();
  const tasks = Promise.allSettled([f.request('/zones', read(a.signal)), f.request('/zones', read(b.signal))]);
  await tick(); a.abort(); b.abort(); await tasks; assert.equal(f.calls[0].init.signal.aborted, true);
  const fresh = f.request('/zones', read()); f.calls[0].resolve('late old');
  const join = f.request('/zones', read()); await tick(); assert.equal(f.calls.length, 2);
  f.calls[1].resolve('current'); assert.deepEqual(await Promise.all([fresh, join]), ['current', 'current']);
});

test('all readers cancelled before dispatch produce no exchange', async () => {
  const f = fixture(), controller = new AbortController();
  const task = f.request('/zones', read(controller.signal)); const rejected = assert.rejects(task, { name: 'AbortError' });
  controller.abort(); await rejected; await tick(); assert.equal(f.calls.length, 0);
});

test('already-cancelled reads and writes do not dispatch', async () => {
  const f = fixture(), controller = new AbortController(); controller.abort();
  await assert.rejects(f.request('/zones', read(controller.signal)), { name: 'AbortError' });
  await assert.rejects(f.request('/pause', { ...write(), signal: controller.signal }), { name: 'AbortError' });
  assert.equal(f.calls.length, 0);
});

test('pending-key capacity is bounded without dropping distinct observations', async () => {
  const f = fixture({ limit: 2 });
  const paths = ['/a', '/b', '/a', '/c', '/c']; const tasks = paths.map((p) => f.request(p, read())); await tick();
  assert.equal(f.calls.length, 4); assert.equal(f.calls.filter((c) => c.where === '/a').length, 1);
  f.calls.forEach((c) => c.resolve(c.where)); assert.deepEqual(await Promise.all(tasks), paths);
});

for (const [label, where, init] of [
  ['events', '/events?since=7&wait=60', read()], ['foreign origin', '//other.invalid/zones', read()],
  ['different headers', '/zones', { ...read(), headers: { Accept: 'application/json', 'X-Test': 'other' } }],
  ['different representation', '/zones', { ...read(), headers: { Accept: 'text/plain' } }],
  ['unknown options', '/zones', { ...read(), mode: 'cors' }], ['different cache', '/zones', { ...read(), cache: 'reload' }],
]) test(`${label} does not participate in sharing`, async () => {
  const f = fixture(); const tasks = [f.request(where, init), f.request(where, init)]; assert.equal(f.calls.length, 2);
  f.calls.forEach((c) => c.resolve(null)); await Promise.all(tasks);
});

test('invalid creation options are rejected', () => {
  assert.throws(() => R.create(null));
  for (const limit of [0, -1, 1.5, 65, Infinity]) assert.throws(() => R.create(() => {}, { limit }));
});

test('real HTTP burst shares one response; later reads and commands stay separate', async (t) => {
  let gets = 0, posts = 0; const release = gate();
  const server = http.createServer(async (req, res) => {
    if (req.method === 'GET') { ++gets; await release.promise; }
    else { ++posts; for await (const _ of req) { /* Drain the command body. */ } }
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ gets, posts }));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const request = R.create(async (where, init) => (await fetch(origin + where, init)).json());
  const burst = Array.from({ length: 50 }, () => request('/zones', read()));
  release.resolve(); await Promise.all(burst); assert.equal(gets, 1);
  await request('/zones', read()); assert.equal(gets, 2);
  await Promise.all([request('/pause', write()), request('/pause', write())]); assert.equal(posts, 2);
  await request('/zones', read()); assert.equal(gets, 3);
});

// A later preflight must not borrow an observation sent before it began.
test('reads arriving after dispatch start a fresh exchange even while the old body is pending', async () => {
  const f = fixture(); const first = f.request('/zones', read()); await tick();
  const second = f.request('/zones', read()); await tick();
  assert.equal(f.calls.length, 2);
  f.calls[0].resolve('older observation'); f.calls[1].resolve('new observation');
  assert.equal(await first, 'older observation'); assert.equal(await second, 'new observation');
});


test('a read cancelled globally cannot succeed from a late ignored-abort reply', async () => {
  const f = fixture();
  const task = f.request('/zones', read());
  const rejected = assert.rejects(task, { name: 'AbortError' });
  await tick(); f.request.cancelReads();
  assert.equal(f.calls[0].init.signal.aborted, true);
  f.calls[0].resolve({ stale: true }); await rejected;
  const fresh = f.request('/zones', read()); await tick();
  f.calls[1].resolve({ current: true }); assert.deepEqual(await fresh, { current: true });
});
