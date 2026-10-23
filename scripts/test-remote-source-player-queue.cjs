// Positive noninterrupting queue workflows through the shipped source player.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const Player = require('../crates/fsonos-api/src/remote/source-player.js');
const ID = '0123456789ABCDEFGHIJKL';
const group = { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Office'] };
const queue = () => ({ zone: 'Den', total: 12, playing: 3, revision: 'sample-revision-1', items: [{ position: 1 }] });
const playback = () => ({ zone: structuredClone(group), transport_state: 'playing',
  track: { uri: 'sample:track-a', queue_position: 3 } });
const added = () => ({ zone: 'Den', first: 4, count: 2, playing_queue: true, done: 'Queued music' });
const defer = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };
function harness() {
  const h = { zones: [group], state: playback(), queue: queue(), response: added(), calls: [], custom: null };
  h.api = Player.model({ call: async (name, args) => {
    h.calls.push([name, structuredClone(args)]);
    if (h.custom) { const v = h.custom(name, args); if (v !== undefined) return v; }
    return structuredClone(name === 'zones' ? h.zones : name === 'state' ? h.state
      : name === 'sourceQueueState' ? h.queue : h.response);
  } });
  h.writes = () => h.calls.filter(([name]) => ['sourceQueue', 'sourcePlay'].includes(name));
  h.open = async (mode = 'next', kind = 'album') => {
    await h.api.refresh(); h.api.edit('target', 'Den@S2'); h.api.edit('input', `https://open.spotify.com/${kind}/${ID}?si=abc`); h.api.edit('mode', mode);
  };
  h.review = async (mode, kind) => { await h.open(mode, kind); assert.equal(await h.api.review(), true); return h.api.state.pending; };
  return h;
}

for (const mode of ['next', 'end']) for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) {
  test(`${kind} queue-${mode} preserves playback and sends only the native add request`, async () => {
    const h = harness(), before = structuredClone(h.state), p = await h.review(mode, kind);
    assert.equal(h.writes().length, 0); assert.equal(p.mode, mode); assert.equal(p.before.queue.total, 12);
    assert.equal(await h.api.confirm(p), true);
    assert.deepEqual(h.writes(), [['sourceQueue', { body: { zone: 'Den@S2', source_uri: `spotify:${kind}:${ID}`, next: mode === 'next' } }]]);
    assert.deepEqual(h.state, before); assert.equal(h.api.state.result.status, 'accepted');
    assert.match(h.api.state.result.detail, /2 track\(s\).*position 4/);
    assert.ok(h.calls.filter(([name]) => name === 'sourceQueueState').every(([, args]) => args.query.offset === 0 && args.query.limit === 1));
  });
}

test('queueing while paused never resumes playback or sets a source', async () => {
  const h = harness(); h.state.transport_state = 'paused'; const p = await h.review('next');
  assert.equal(await h.api.confirm(p), true); assert.equal(h.writes()[0][0], 'sourceQueue');
  assert.equal(h.state.transport_state, 'paused');
});

test('an idle or empty queue can be extended without claiming that music started', async () => {
  for (const total of [0, 12]) {
    const h = harness(); h.queue = { ...queue(), total, playing: null, items: total ? [{ position: 1 }] : [] };
    h.state.track = null; h.response = { ...added(), first: total + 1, playing_queue: false };
    const p = await h.review('next'); assert.equal(p.before.queue.playing, null);
    assert.equal(await h.api.confirm(p), true); assert.match(h.api.state.result.detail, /did not start/);
    assert.equal(h.writes().length, 1);
  }
});

test('streams cannot silently switch a queue request into immediate playback', async () => {
  const h = harness(); await h.open('end'); h.api.edit('input', 'https://audio.example/live');
  const n = h.calls.length; assert.equal(await h.api.review(), false); assert.equal(h.calls.length, n);
  assert.equal(h.api.state.mode, 'end'); assert.match(h.api.state.message, /Choose Play now/); assert.equal(h.writes().length, 0);
});

test('changing insertion mode consumes a pending review and cannot reuse its handler', async () => {
  const h = harness(), p = await h.review('next'); h.api.edit('mode', 'end');
  assert.equal(h.api.state.pending, null); assert.equal(await h.api.confirm(p), false);
  assert.equal(h.api.edit('mode', 'unknown'), false); assert.equal(h.api.state.mode, 'end');
  assert.equal(h.writes().length, 0);
});

test('changed queue contents or revision require a fresh review in either mode', async () => {
  for (const mode of ['end', 'next']) for (const change of [q => { q.revision += 'x'; }, q => { q.total++; }]) {
    const h = harness(), p = await h.review(mode); change(h.queue);
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0); assert.equal(h.api.state.result.status, 'not_sent');
  }
});

test('queue-next refuses a moving insertion anchor but appending permits natural track advances', async () => {
  for (const mode of ['next', 'end']) {
    const h = harness(), p = await h.review(mode); h.queue.playing = 4; h.state.track.queue_position = 4; h.state.track.uri = 'sample:track-b';
    assert.equal(await h.api.confirm(p), mode === 'end'); assert.equal(h.writes().length, mode === 'end' ? 1 : 0);
  }
});

test('malformed queue identity, count or first-page shape is never a valid review', async () => {
  for (const patch of [{ zone: 'Other' }, { revision: null }, { revision: '' }, { total: -1 }, { total: 1.5 },
    { total: 0xffffffff + 1 }, { items: [] }, { items: [{ position: 2 }] }, { playing: 0 }, { playing: 13 }]) {
    const h = harness(); h.queue = { ...h.queue, ...patch }; await h.open();
    assert.equal(await h.api.review(), false); assert.equal(h.writes().length, 0);
  }
});

test('a mismatched group is rejected before a queue-add request', async () => {
  const h = harness(), p = await h.review(); h.zones = [{ ...group, members: ['Den'] }];
  assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
});

test('delayed queue snapshots cannot revive edited or closed source reviews', async () => {
  const h = harness(), wait = defer(); await h.open(); h.custom = name => name === 'sourceQueueState' ? wait.promise : undefined;
  const reviewing = h.api.review(); await settle(); h.api.edit('mode', 'play'); wait.resolve(h.queue);
  assert.equal(await reviewing, false); assert.equal(h.api.state.pending, null); assert.equal(h.writes().length, 0);
});

test('lost queue acknowledgements never replay or fall back to playback', async () => {
  const h = harness(), p = await h.review(); h.custom = name => { if (name === 'sourceQueue') throw new Error('reply lost'); };
  assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain');
  h.api.close(); await h.api.refresh(); assert.equal(await h.api.confirm(p), false);
  assert.equal(h.writes().length, 1); assert.equal(h.writes()[0][0], 'sourceQueue'); assert.match(h.api.state.result.detail, /not repeated/);
});

test('incomplete or inconsistent queue receipts remain uncertain rather than inventing success', async () => {
  for (const patch of [{ zone: 'Other' }, { first: 0 }, { count: 0 }, { count: -1 }, { first: 0xffffffff, count: 2 },
    { playing_queue: 'yes' }, { done: '' }]) {
    const h = harness(), p = await h.review(); h.response = { ...added(), ...patch };
    assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain'); assert.equal(h.writes().length, 1);
  }
});

test('queue insertion is single-flight through close and reopen, and retains its late receipt', async () => {
  const h = harness(), p = await h.review(), wait = defer(); h.custom = name => name === 'sourceQueue' ? wait.promise : undefined;
  const writing = h.api.confirm(p); await settle(); assert.equal(await h.api.confirm(p), false);
  h.api.close(); assert.equal(await h.api.refresh(), false); assert.equal(h.api.edit('mode', 'play'), false);
  wait.resolve(added()); assert.equal(await writing, true); assert.equal(h.writes().length, 1); assert.equal(h.api.state.result.status, 'accepted');
});

test('optional title and next flag use the queue contract without playback-only fields', async () => {
  const h = harness(); await h.open('next', 'playlist'); h.api.edit('title', 'Saved for later'); await h.api.review();
  await h.api.confirm(h.api.state.pending);
  assert.deepEqual(h.writes()[0][1].body, { zone: 'Den@S2', source_uri: `spotify:playlist:${ID}`, title: 'Saved for later', next: true });
});

test('real loopback queue addition leaves the existing track paused', async (t) => {
  const received = []; const now = playback(); now.transport_state = 'paused'; const q = queue();
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const part of req) chunks.push(part);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null;
    received.push([req.method, req.url, body]);
    const result = req.method === 'POST' ? added() : req.url === '/zones' ? [group] : req.url.includes('/queue?') ? q : now;
    res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(result));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening'); t.after(() => { server.closeAllConnections(); server.close(); });
  const origin = `http://127.0.0.1:${server.address().port}`;
  const api = Player.model({ call: async (name, args = {}) => {
    const route = Player.routes[name] || (name === 'zones' ? ['GET', '/zones'] : ['GET', '/zones/{room}/state']);
    const url = route[1].replace('{room}', encodeURIComponent(args.params?.room));
    const init = route[0] === 'POST' ? { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(args.body) } : {};
    return (await fetch(origin + url + (args.query ? '?' + new URLSearchParams(args.query) : ''), init)).json();
  } });
  await api.refresh(); api.edit('target', 'Den@S2'); api.edit('input', `spotify:playlist:${ID}`); api.edit('mode', 'end');
  await api.review(); assert.equal(await api.confirm(api.state.pending), true);
  assert.deepEqual(received.filter(([method]) => method === 'POST'), [['POST', '/queue', { zone: 'Den@S2', source_uri: `spotify:playlist:${ID}`, next: false }]]);
  assert.equal(now.transport_state, 'paused'); assert.equal(now.track.uri, 'sample:track-a');
  assert.ok(received.some(([, url]) => url === '/zones/Den%40S2/queue?offset=0&limit=1'));
});
