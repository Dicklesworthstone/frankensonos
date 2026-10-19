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

const sourcePlayer = require('../crates/fsonos-api/src/remote/source-player.js');
const groups = () => [
  { household: 'S1', coordinator_room: 'Study', members: ['Study', 'Hall'] },
  { household: 'S2', coordinator_room: 'Study', members: ['Study'] },
];
const groupOf = (target) => groups().find((g) => `${g.coordinator_room}@${g.household}` === target);
const tick = () => new Promise((resolve) => setImmediate(resolve));
function playerFixture(extra = {}) {
  const calls = [];
  const m = sourcePlayer.model({ execute: extra.execute, call: async (name, args) => {
    calls.push([name, args]);
    if (extra.call) return extra.call(name, args);
    if (name === 'zones') return groups();
    if (name === 'state') return { zone: groupOf(args.params.room), transport_state: 'playing', track: { uri: 'test:current', title: 'Current', queue_position: 1 } };
    if (name === 'sourceQueueState') return { zone: 'Study', revision: 'queue-original', total: 3, playing: 1, items: [{ position: 1 }] };
    if (name === 'sourceQueue') return { zone: 'Study', done: 'Added', first: 2, count: 1, playing_queue: true };
    if (name === 'sourcePlay') return { done: 'Playing', changed: true };
    throw new Error(`Unexpected call ${name}`);
  } });
  return { m, calls, writes: () => calls.filter(([name]) => ['sourceQueue', 'sourcePlay'].includes(name)) };
}

test('only a current visible history row can initiate the handoff', async () => {
  const { m } = fixture(); await m.refresh(); const row = m.state.rows[0];
  assert.equal(m.pick(row), row); assert.equal(m.pick({ ...row }), null);
  m.search('not loaded'); assert.equal(m.pick(row), null); m.search('');
  m.close(); assert.equal(m.pick(row), null); await m.refresh(); assert.equal(m.pick(row), null);
  const fresh = m.state.rows[0]; m.window(100); assert.equal(m.pick(fresh), null);
});

test('history handoff transfers only the supported source and an optional bounded title', () => {
  for (const kind of ['track', 'album', 'playlist']) {
    const row = play({ source_uri: `spotify:${kind}:0000000000000000000001` });
    const incoming = history.replayOf(row, sourcePlayer.source);
    assert.deepEqual(incoming, { source_uri: row.source_uri, title: row.title });
    assert.ok(Object.isFrozen(incoming));
    assert.equal(Object.hasOwn(incoming, 'room'), false);
    assert.equal(Object.hasOwn(incoming, 'played_at'), false);
  }
  for (const title of [null, 'x'.repeat(257), 'unsafe\nline']) {
    assert.deepEqual(history.replayOf(play({ title }), sourcePlayer.source), { source_uri: play().source_uri });
  }
  const uri = 'https://audio.example.org/Stream%2FName?a=Literal%2BValue';
  assert.equal(history.replayOf(play({ source_uri: uri }), sourcePlayer.source).source_uri, uri);
});

test('unsupported historical sources are never guessed into playable tracks or fetched', async () => {
  for (const source_uri of ['x-rincon:old-speaker', 'x-sonos-spotify:opaque-renderer', 'javascript:alert(1)',
    'spotify:artist:0000000000000000000001', 'spotify:episode:0000000000000000000001', 'https://spotify.link/short']) {
    const f = playerFixture();
    assert.throws(() => history.replayOf(play({ source_uri }), sourcePlayer.source));
    assert.equal(await f.m.offer({ source_uri }), false); assert.equal(f.calls.length, 0);
  }
});

test('source imports cannot carry a destination, queue position, action or unknown fields', async () => {
  for (const value of [null, [], {}, { ...play() }, { source_uri: play().source_uri, zone: 'Study@S2' },
    { source_uri: play().source_uri, next: true }, { source_uri: play().source_uri, mode: 'play' },
    { source_uri: play().source_uri, title: 1 }, { source_uri: play().source_uri, title: 'x'.repeat(257) }]) {
    const f = playerFixture(); assert.equal(await f.m.offer(value), false); assert.equal(f.calls.length, 0);
  }
});

for (const gen of ['S1', 'S2']) for (const mode of ['play', 'next', 'end']) {
  test(`recorded source reaches ${gen} ${mode} only after selecting, reviewing and confirming`, async () => {
    const f = playerFixture(), row = play({ room: 'Study', source_uri: 'spotify:album:0000000000000000000001' });
    assert.equal(await f.m.offer(history.replayOf(row, sourcePlayer.source)), true);
    assert.equal(f.m.state.target, ''); assert.equal(f.m.state.mode, 'play');
    assert.equal(await f.m.review(), false); assert.equal(f.writes().length, 0);
    assert.deepEqual(f.calls.map(([n]) => n), ['zones']);
    assert.equal(f.m.edit('target', `Study@${gen}`), true); f.m.edit('mode', mode);
    assert.equal(await f.m.review(), true); const review = f.m.state.pending;
    assert.equal(f.writes().length, 0);
    assert.equal(await f.m.confirm(review), true); assert.equal(await f.m.confirm(review), false);
    assert.equal(f.writes().length, 1);
    assert.deepEqual(f.writes()[0], [mode === 'play' ? 'sourcePlay' : 'sourceQueue', { body: {
      zone: `Study@${gen}`, source_uri: row.source_uri, title: row.title, ...(mode === 'play' ? {} : { next: mode === 'next' }),
    } }]);
    assert.ok(f.calls.filter(([n]) => n === 'state').every(([, a]) => a.params.room === `Study@${gen}`));
  });
}

test('existing draft and review are preserved until the owner explicitly clears them', async () => {
  const f = playerFixture(); await f.m.refresh(); f.m.edit('target', 'Study@S1');
  f.m.edit('input', 'spotify:track:0000000000000000000002'); await f.m.review();
  const before = f.m.state.pending, calls = f.calls.length;
  assert.equal(await f.m.offer({ source_uri: play().source_uri }), false);
  assert.equal(f.m.state.pending, before); assert.equal(f.calls.length, calls);
  assert.equal(f.m.clearDraft(), true); assert.equal(await f.m.confirm(before), false);
  assert.equal(await f.m.offer({ source_uri: play().source_uri }), true);
  assert.equal(f.m.state.target, ''); assert.equal(f.writes().length, 0);
});

test('closure or a user edit during imported destination discovery defeats late replies', async () => {
  for (const change of ['close', 'clearDraft', 'dispose']) {
    const wait = delayed(), f = playerFixture({ call: () => wait.promise });
    const offering = f.m.offer({ source_uri: play().source_uri }); f.m[change](); wait.resolve(groups());
    assert.equal(await offering, false); assert.equal(f.m.state.target, ''); assert.equal(f.writes().length, 0);
  }
});

test('a dispatched command retains its slot across another offered source or draft clearing', async () => {
  const hold = delayed(); let writes = 0;
  const f = playerFixture({ call: async (name, args) => {
    if (name === 'zones') return groups();
    if (name === 'state') return { zone: groupOf(args.params.room), transport_state: 'stopped' };
    writes++; return hold.promise;
  } });
  await f.m.offer({ source_uri: play().source_uri }); f.m.edit('target', 'Study@S2'); await f.m.review();
  const saving = f.m.confirm(f.m.state.pending); await tick(); assert.equal(writes, 1);
  assert.equal(await f.m.offer({ source_uri: 'spotify:track:0000000000000000000002' }), false);
  assert.equal(f.m.clearDraft(), false); assert.equal(f.m.state.writing, true);
  hold.resolve({ done: 'Played', changed: true }); assert.equal(await saving, true); assert.equal(writes, 1);
});

test('a changed destination or queue revision still rejects the imported source before writing', async () => {
  for (const change of ['group', 'queue']) {
    let drift = false, writes = 0;
    const f = playerFixture({ call: async (name, args) => {
      if (name === 'zones') return drift && change === 'group' ? [groups()[1]] : groups();
      if (name === 'state') return { zone: groups()[0], transport_state: 'playing', track: { uri: 'test:current', queue_position: 1 } };
      if (name === 'sourceQueueState') return { zone: 'Study', revision: drift ? 'new-revision' : 'old-revision', total: 3, playing: 1, items: [{ position: 1 }] };
      writes++; throw new Error('must not send');
    } });
    await f.m.offer({ source_uri: play().source_uri }); f.m.edit('target', 'Study@S1'); f.m.edit('mode', 'next');
    await f.m.review(); drift = true; assert.equal(await f.m.confirm(f.m.state.pending), false);
    assert.equal(writes, 0); assert.equal(f.m.state.result.status, 'not_sent');
  }
});

test('uncertain replay is not retried and clearing a draft retains its outcome evidence', async () => {
  let writes = 0;
  const f = playerFixture({ call: async (name, args) => {
    if (name === 'zones') return groups();
    if (name === 'state') return { zone: groupOf(args.params.room), transport_state: 'stopped' };
    writes++; throw new Error('connection ended after send');
  } });
  await f.m.offer({ source_uri: play().source_uri }); f.m.edit('target', 'Study@S2'); await f.m.review();
  const reviewed = f.m.state.pending;
  assert.equal(await f.m.confirm(reviewed), false); assert.equal(await f.m.confirm(reviewed), false);
  assert.equal(writes, 1); assert.equal(f.m.state.result.status, 'uncertain');
  const receipt = f.m.state.result; f.m.clearDraft(); assert.equal(f.m.state.result, receipt);
});

test('the existing shared command lane can stop a historical replay before dispatch', async () => {
  const f = playerFixture({ execute: async () => { throw new Error('another action is busy'); } });
  await f.m.offer({ source_uri: play().source_uri }); f.m.edit('target', 'Study@S2'); await f.m.review();
  assert.equal(await f.m.confirm(f.m.state.pending), false); assert.equal(f.writes().length, 0);
  assert.equal(f.m.state.result.status, 'not_sent');
});

test('real HTTP history-to-queue flow keeps literal source and selected household with one write', async (t) => {
  const seen = [], row = play({ room: 'Study', title: 'Literal <&> 夜' });
  const server = http.createServer(async (req, res) => {
    let text = ''; for await (const chunk of req) text += chunk;
    const body = text ? JSON.parse(text) : null;
    seen.push([req.method, req.url, body]);
    let answer;
    if (req.url.startsWith('/history')) answer = [row];
    else if (req.url === '/zones') answer = groups();
    else if (req.url.endsWith('/state')) answer = { zone: groups()[1], transport_state: 'stopped' };
    else if (req.method === 'GET') answer = { zone: 'Study', total: 0, revision: 'empty', items: [] };
    else answer = { zone: 'Study', done: 'Queued', first: 1, count: 1, playing_queue: false };
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(answer));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const routes = { ...history.routes, ...sourcePlayer.routes, zones: ['GET', '/zones'], state: ['GET', '/zones/{room}/state'] };
  const call = async (name, { query, params = {}, body, signal } = {}) => {
    const [method, pattern] = routes[name];
    const path = pattern.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    return (await fetch(`http://127.0.0.1:${server.address().port}${path}${query ? '?' + new URLSearchParams(query) : ''}`, {
      method, signal, headers: { 'Content-Type': 'application/json' }, ...(body ? { body: JSON.stringify(body) } : {}),
    })).json();
  };
  const h = history.model({ call }), p = sourcePlayer.model({ call });
  await h.refresh(); await p.offer(history.replayOf(h.pick(h.state.rows[0]), sourcePlayer.source));
  assert.equal(seen.some(([method]) => method !== 'GET'), false);
  p.edit('target', 'Study@S2'); p.edit('mode', 'end'); await p.review(); await p.confirm(p.state.pending);
  assert.deepEqual(seen.filter(([method]) => method !== 'GET'), [['POST', '/queue', { zone: 'Study@S2', source_uri: row.source_uri, title: row.title, next: false }]]);
  assert.match(p.state.result.detail, /did not start/);
});
