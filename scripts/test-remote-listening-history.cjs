'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const history = require('../crates/fsonos-api/src/remote/listening-history.js');
const play = (extra = {}) => ({ source_uri: 'spotify:track:0000000000000000000001', played_at: 1792360800,
  title: '<Whole work & song>', artist: 'Earth, Wind & Fire', room: 'Study', ...extra });
const delayed = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const fixture = (call = async () => [play()]) => {
  const calls = [];
  const m = history.model({ now: () => 1234, call: (...args) => { calls.push(args); return call(...args); } });
  return { m, calls };
};

test('lazy read-only model uses the public history route and an explicit bounded window', async () => {
  const { m, calls } = fixture();
  assert.deepEqual(history.routes, { listeningHistory: ['GET', '/history'] });
  assert.equal(calls.length, 0);
  assert.equal(await m.refresh(), true);
  assert.equal(calls[0][0], 'listeningHistory');
  assert.deepEqual(calls[0][1].query, { limit: 20 });
  assert.equal(calls[0][1].signal.aborted, false);
  assert.equal(m.state.observedAt, 1234);
  assert.equal(m.state.stale, false);
  m.search('fire'); assert.equal(calls.length, 1);
});

test('preserves duplicate listens and server order despite nonmonotonic timestamps', () => {
  const input = [play(), play({ played_at: 1 }), play()];
  const rows = history.playsOf(input, 20);
  assert.equal(rows.length, 3);
  assert.deepEqual(rows.map((r) => r.played_at), [1792360800, 1, 1792360800]);
  input[0].title = 'Changed'; assert.equal(rows[0].title, '<Whole work & song>');
  assert.throws(() => rows.push(play()));
  assert.throws(() => { rows[0].title = 'Changed'; });
});

test('unknown metadata and renderer sources remain visible without inventing identities', () => {
  const rows = history.playsOf([{ source_uri: 'x-rincon:synthetic-old-group', played_at: 0 }], 20);
  assert.deepEqual(rows[0], { source_uri: 'x-rincon:synthetic-old-group', played_at: 0, title: null, artist: null, room: null });
  assert.equal(history.playsOf([play({ title: null, room: null, artist: null })], 20)[0].room, null);
});

test('malformed and oversized snapshots fail as a whole', () => {
  for (const value of [null, {}, Array(21).fill(play()), [null], [play({ source_uri: '' })],
    [play({ played_at: '123' })], [play({ played_at: Infinity })], [play({ played_at: 8640000000001 })],
    [play({ room: {} })], [play({ title: ['x'] })], [play({ artist: 4 })], [play({ title: 'x'.repeat(65537) })]]) {
    assert.throws(() => history.playsOf(value, 20));
  }
  assert.throws(() => history.playsOf([], 1000));
});

test('literal title artist source and room filtering is local and does not collapse households', () => {
  const rows = history.playsOf([play(), play({ room: 'Hall', title: 'Björk / 夜' }), play({ room: null })], 20);
  assert.equal(history.visible(rows, 'wind & fire').length, 3);
  assert.equal(history.visible(rows, '夜', 'Hall').length, 1);
  assert.equal(history.visible(rows, '0000000001').length, 3);
  assert.equal(history.visible(rows, '', 'Study').length, 1);
  assert.equal(history.visible(rows, 'no such title').length, 0);
});

test('window changes discard older results, including a late reply ignoring abort', async () => {
  const old = delayed(); let n = 0;
  const { m, calls } = fixture(() => ++n === 1 ? old.promise : Promise.resolve([play({ title: 'New' })]));
  const first = m.refresh();
  assert.equal(m.window(100), true); assert.equal(calls[0][1].signal.aborted, true);
  assert.equal(await m.refresh(), true);
  old.resolve([play({ title: 'Old' })]); assert.equal(await first, false);
  assert.equal(m.state.rows[0].title, 'New');
  assert.deepEqual(calls[1][1].query, { limit: 100 });
});

test('a later refresh wins and an old error cannot replace its outcome', async () => {
  const old = delayed(); let n = 0;
  const { m } = fixture(() => ++n === 1 ? old.promise : Promise.resolve([]));
  const first = m.refresh(); await m.refresh(); old.reject(new Error('obsolete failure'));
  assert.equal(await first, false); assert.equal(m.state.error, ''); assert.equal(m.state.loaded, true);
});

for (const stop of ['close', 'dispose']) test(`${stop} aborts reads and rejects late results`, async () => {
  const wait = delayed(), { m, calls } = fixture(() => wait.promise);
  const pending = m.refresh(); m[stop](); assert.equal(calls[0][1].signal.aborted, true);
  wait.resolve([play()]); assert.equal(await pending, false); assert.equal(m.state.loaded, false);
  assert.equal(m.state.open, false); assert.equal(m.state.loading, false);
  if (stop === 'dispose') { assert.equal(await m.refresh(), false); assert.equal(calls.length, 1); }
});

test('failed refreshes preserve a visibly stale snapshot with policy hints', async () => {
  let fail = false;
  const { m } = fixture(async () => { if (fail) throw Object.assign(new Error('POLICY_DENIED'), { hint: 'Allow recent_plays.' }); return [play()]; });
  await m.refresh(); const before = m.state.rows; fail = true;
  assert.equal(await m.refresh(), false); assert.equal(m.state.rows, before);
  assert.equal(m.state.stale, true); assert.equal(m.state.error, 'POLICY_DENIED Allow recent_plays.');
});

test('invalid windows and filters send nothing and do not discard good state', async () => {
  const { m, calls } = fixture(); await m.refresh(); const before = m.state.rows;
  for (const n of [0, -1, 501, 1000, 1.5, '100', null]) assert.equal(m.window(n), false);
  assert.equal(m.search('x'.repeat(2049)), false); assert.equal(m.search([], ''), false);
  assert.equal(m.state.rows, before); assert.equal(calls.length, 1);
});

test('presentation errors cannot turn a successful read into an application failure', async () => {
  const m = history.model({ call: async () => [play()], changed: () => { throw new Error('view failed'); } });
  assert.equal(await m.refresh(), true); assert.equal(m.state.loaded, true);
});

test('real loopback HTTP returns exact literal records and sends only GET history', async (t) => {
  const seen = [];
  const expected = [play({ title: 'Étude <&> 夜' }), play({ title: 'Étude <&> 夜' })];
  const server = http.createServer((req, res) => {
    seen.push([req.method, req.url]); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(expected));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const m = history.model({ call: async (name, { query, signal }) => {
    const [method, path] = history.routes[name];
    return (await fetch(`http://127.0.0.1:${server.address().port}${path}?${new URLSearchParams(query)}`, { method, signal })).json();
  } });
  m.window(100); assert.equal(await m.refresh(), true); assert.deepEqual(m.state.rows, expected);
  assert.deepEqual(seen, [['GET', '/history?limit=100']]); m.dispose();
});
