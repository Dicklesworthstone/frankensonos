'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const remote = require('../crates/fsonos-api/src/remote/library-sync.js');

const completed = (overrides = {}) => ({ at: 1790000000, tracks: 100, candidates: 80,
  classical: 12, retired: 3, ...overrides });
const status = (overrides = {}) => ({ done: 'Library read.', running: false, last: completed(), ...overrides });
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = () => new Promise((resolve) => setImmediate(resolve));

function harness(handler = () => status()) {
  const calls = [], timers = new Map(); let serial = 0;
  const model = remote.model({ call: async (name, options) => { calls.push([name, options]); return handler(name, options); },
    schedule: (fn, delay) => { const id = ++serial; timers.set(id, { fn, delay }); return id; },
    cancel: (id) => timers.delete(id) });
  return { model, calls, timers, fire() { assert.equal(timers.size, 1); const [id, t] = [...timers][0]; timers.delete(id); t.fn(); return t.delay; } };
}

test('opening only observes; neither construction nor observation starts a refresh', async () => {
  const h = harness(); assert.equal(h.calls.length, 0); assert.equal(await h.model.start(), false);
  await h.model.open(); assert.deepEqual(h.calls, [['librarySyncStatus', undefined]]);
  assert.equal(h.model.state.fresh, true); assert.equal(h.timers.size, 0);
});

test('old completed DTOs keep omitted notes and pending genres at their defaults', () => {
  const s = remote.statusOf(status()); assert.deepEqual(s.last.notes, []); assert.equal(s.last.genre_artists_pending, 0);
  assert.equal(remote.statusOf({ done: 'Not read yet', running: false }).last, null);
  assert.ok(Object.isFrozen(s) && Object.isFrozen(s.last) && Object.isFrozen(s.last.notes));
});

test('partial results, previous completion and latest failure stay distinct', () => {
  const raw = status({ running: true, error: 'The last attempt failed',
    last: completed({ genre_artists_pending: 4, notes: ['Missing taste scope', '<script>unread album</script>'] }) });
  const s = remote.statusOf(raw); raw.last.notes.push('later');
  assert.equal(s.running, true); assert.equal(s.last.genre_artists_pending, 4);
  assert.equal(s.last.notes.length, 2); assert.equal(s.error, 'The last attempt failed');
  assert.equal(s.last.notes[1], '<script>unread album</script>');
});

test('unreadable DTOs never authorize a refresh', async () => {
  for (const value of [null, [], {}, { running: false }, status({ running: 'false' }), status({ error: {} }),
    status({ last: [] }), status({ last: completed({ tracks: -1 }) }),
    status({ last: completed({ candidates: 101 }) }), status({ last: completed({ classical: 81 }) }),
    status({ last: completed({ tracks: 2 ** 53 }) }), status({ last: completed({ at: 8640000000001 }) }),
    status({ last: completed({ notes: 'not a list' }) }), status({ last: completed({ notes: [5] }) }),
    status({ last: completed({ genre_artists_pending: 1.2 }) })]) {
    assert.throws(() => remote.statusOf(value)); const h = harness(() => value);
    assert.equal(await h.model.open(), false); assert.equal(await h.model.start(), false);
    assert.equal(h.calls.length, 1); h.model.dispose();
  }
});

test('running jobs are observed until completion without an invented progress percentage', async () => {
  let running = true; const h = harness(() => status({ running })); await h.model.open();
  assert.equal(await h.model.start(), false); assert.equal(h.fire(), remote.POLL_MS); await settle();
  assert.equal(h.timers.size, 1); running = false; h.fire(); await settle();
  assert.equal(h.model.state.status.running, false); assert.equal(h.timers.size, 0);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus'));
});

test('the explicit start sends exactly one empty JSON body', async () => {
  const h = harness((name) => status({ running: name === 'librarySyncStart' })); await h.model.open();
  assert.equal(await h.model.start(), true);
  assert.deepEqual(h.calls[1], ['librarySyncStart', { body: {} }]);
  assert.equal(await h.model.start(), false); assert.equal(h.calls.length, 2);
});

test('double clicks and status requests cannot duplicate an in-flight write', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const first = h.model.start();
  assert.equal(await h.model.start(), false); assert.equal(await h.model.refresh(), false);
  assert.equal(h.calls.length, 2); wait.resolve(status({ running: true }));
  assert.equal(await first, true); await settle();
  assert.equal(h.calls.filter(([name]) => name === 'librarySyncStart').length, 1);
});

test('a lost write reply is followed by status reads, not an automatic retry', async () => {
  const h = harness((name) => { if (name === 'librarySyncStart') throw new Error('Lost reply'); return status(); });
  await h.model.open(); assert.equal(await h.model.start(), false); await settle();
  assert.deepEqual(h.calls.map(([name]) => name), ['librarySyncStatus', 'librarySyncStart', 'librarySyncStatus']);
  assert.equal(h.model.state.uncertain, true); assert.match(h.model.state.message, /nothing was retried/);
  assert.equal(h.model.state.fresh, true); assert.equal(h.timers.size, 0);
});

test('a malformed write reply is uncertain, never mistaken for completion', async () => {
  const h = harness((name) => name === 'librarySyncStart' ? {} : status({ last: null }));
  await h.model.open(); assert.equal(await h.model.start(), false); await settle();
  assert.equal(h.model.state.uncertain, true); assert.equal(h.model.state.status.last, null);
  assert.equal(h.calls.filter(([name]) => name === 'librarySyncStart').length, 1);
});

test('failed observations retain only explicitly stale results and use capped backoff', async () => {
  let fail = false; const h = harness(() => { if (fail) throw new Error('Offline'); return status({ running: true }); });
  await h.model.open(); const previous = h.model.state.status; fail = true;
  await h.model.refresh(); assert.equal(h.model.state.status, previous); assert.equal(h.model.state.fresh, false);
  assert.equal(await h.model.start(), false); const delays = [];
  for (let i = 0; i < 7; i++) { delays.push(h.fire()); await settle(); }
  assert.deepEqual(delays, [6000, 12000, 24000, 48000, 60000, 60000, 60000]);
  fail = false; await h.model.refresh(); assert.equal([...h.timers.values()][0].delay, remote.POLL_MS);
  h.model.dispose();
});

test('policy and unavailable-engine hints remain actionable', async () => {
  const h = harness(() => { throw Object.assign(new Error('No Spotify app configured'), { code: 'NOT_IMPLEMENTED', hint: 'Configure the daemon client ID.' }); });
  await h.model.open(); assert.match(h.model.state.message, /Configure the daemon client ID/);
  assert.equal(await h.model.start(), false); h.model.dispose();
});

test('closing stops observation without pretending to stop the daemon job', async () => {
  const h = harness(() => status({ running: true })); await h.model.open(); h.model.close();
  assert.equal(h.timers.size, 0); assert.equal(h.model.state.status.running, true);
  assert.equal(await h.model.refresh(), false); assert.equal(await h.model.start(), false);
  await h.model.open(); assert.equal(h.calls.length, 2); h.model.dispose();
});

test('a late read cannot reopen a closed panel or publish fresh stale state', async () => {
  const wait = deferred(); const h = harness(() => wait.promise); const opening = h.model.open();
  h.model.close(); wait.resolve(status()); assert.equal(await opening, false);
  assert.equal(h.model.state.open, false); assert.equal(h.model.state.status, null); assert.equal(h.timers.size, 0);
});

test('close/open bursts coalesce to one pending read, with one fresh read afterwards', async () => {
  const wait = deferred(); let first = true;
  const h = harness(() => { if (first) { first = false; return wait.promise; } return status({ done: 'Fresh' }); });
  const opening = h.model.open();
  for (let i = 0; i < 8; i++) { h.model.close(); await h.model.open(); }
  assert.equal(h.calls.length, 1); wait.resolve(status({ done: 'Obsolete' }));
  assert.equal(await opening, false); await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.model.state.status.done, 'Fresh');
});

test('background tabs stop polling and returning performs only a status read', async () => {
  const h = harness(() => status({ running: true })); await h.model.open();
  h.model.setVisible(false); assert.equal(h.timers.size, 0); assert.equal(await h.model.start(), false);
  h.model.setVisible(true); await settle(); assert.equal(h.calls.length, 2);
  assert.ok(h.calls.every(([name]) => name === 'librarySyncStatus')); h.model.dispose();
});

test('initially hidden tabs do not start network work', async () => {
  const h = harness(); h.model.setVisible(false); await h.model.open(); assert.equal(h.calls.length, 0);
  h.model.setVisible(true); await settle(); assert.equal(h.calls.length, 1);
});

test('late writes after closing do not schedule reads or another write', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const starting = h.model.start(); h.model.close();
  wait.resolve(status({ running: true })); await starting; await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.timers.size, 0); assert.equal(h.model.state.fresh, false);
});

test('reopening during a write waits, then re-observes without dispatching again', async () => {
  const wait = deferred(); const h = harness((name) => name === 'librarySyncStart' ? wait.promise : status());
  await h.model.open(); const starting = h.model.start(); h.model.close(); await h.model.open();
  assert.equal(h.calls.length, 2); wait.resolve(status({ running: true })); await starting; await settle();
  assert.equal(h.calls.length, 3); assert.equal(h.calls[2][0], 'librarySyncStatus');
});

test('disposal invalidates all late reads and timers', async () => {
  const wait = deferred(); const h = harness(() => wait.promise); const reading = h.model.open(); h.model.dispose();
  wait.resolve(status({ running: true })); await reading; await settle();
  assert.equal(await h.model.open(), false); assert.equal(h.calls.length, 1); assert.equal(h.timers.size, 0);
});

test('a failed view callback cannot turn an acknowledged write into a retry', async () => {
  const calls = []; const m = remote.model({ call: async (name) => { calls.push(name); return status(); }, changed: () => { throw new Error('View failure'); } });
  await m.open(); assert.equal(await m.start(), true); assert.deepEqual(calls, ['librarySyncStatus', 'librarySyncStart']); m.dispose();
});

test('the registered routes dispatch through actual loopback HTTP with JSON', async (t) => {
  const requests = []; const server = http.createServer(async (req, res) => {
    let body = ''; for await (const bytes of req) body += bytes;
    requests.push([req.method, req.url, req.headers['content-type'], body]);
    res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(status()));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const m = remote.model({ call: async (name, opts) => {
    const [method, path] = remote.routes[name];
    const reply = await fetch(`http://127.0.0.1:${server.address().port}${path}`, { method,
      ...(method === 'POST' ? { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(opts.body) } : {}) });
    return reply.json();
  } });
  await m.open(); assert.equal(await m.start(), true); m.dispose();
  assert.deepEqual(requests, [['GET', '/dj/sync', undefined, ''], ['POST', '/dj/sync', 'application/json', '{}']]);
});

test('the write error hint survives a subsequent successful observation', async () => {
  const h = harness((name) => { if (name === 'librarySyncStart') throw Object.assign(new Error('Sign in first'), { hint: 'Use Spotify setup on the daemon.' }); return status(); });
  await h.model.open(); await h.model.start(); await settle();
  assert.match(h.model.state.message, /Sign in first/); assert.match(h.model.state.message, /Spotify setup/);
});

test('closing from a view callback before dispatch prevents the write', async () => {
  const calls = []; let m;
  m = remote.model({ call: async (name) => { calls.push(name); return status(); }, changed: (s) => { if (s.open && s.busy) m.close(); } });
  await m.open(); assert.equal(await m.start(), false); assert.deepEqual(calls, ['librarySyncStatus']); m.dispose();
});

test('the shipped script and style are embedded and retain the no-external-code policy', () => {
  const fs = require('node:fs'), path = require('node:path');
  const root = path.resolve(__dirname, '..');
  const embed = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  for (const [file, name] of [['library-sync.js','SYNC_SCRIPT'],['library-sync.css','SYNC_STYLE']]) {
    assert.ok(embed.includes(`include_str!("../remote/${file}")`));
    assert.ok(embed.includes(`{${name}}`)); assert.ok(embed.includes(`("${file}", ${name})`));
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', file), 'utf8');
    for (const banned of ['innerHTML','localStorage','sessionStorage','eval(', 'http://','https://']) assert.ok(!source.includes(banned), `${file}: ${banned}`);
  }
  assert.ok(embed.includes("connect-src 'self'; base-uri 'none'; form-action 'none';"));
});
