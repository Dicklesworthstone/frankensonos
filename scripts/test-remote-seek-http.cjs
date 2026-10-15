// Real loopback HTTP around the shipped seek controller; no speakers or Spotify.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const Playback = require('../crates/fsonos-api/src/remote/playback.js');
const clone = (v) => JSON.parse(JSON.stringify(v));
const z = { household: 'S1', coordinator_room: 'A & B/?#', members: ['A & B/?#'] };
const sample = () => ({ zone: clone(z), transport_state: 'paused',
  track: { uri: 'renderer:synthetic', queue_position: 1, position_secs: 20, duration_secs: 14400 } });
function deferred() {
  let resolve;
  const promise = new Promise((yes) => { resolve = yes; });
  return { promise, resolve };
}
async function server(t, handler) {
  const s = http.createServer(handler);
  await new Promise((resolve, reject) => { s.once('error', reject); s.listen(0, '127.0.0.1', resolve); });
  t.after(() => new Promise((resolve) => { s.close(resolve); s.closeAllConnections(); }));
  return `http://127.0.0.1:${s.address().port}`;
}
function reply(res, value, status = 200) {
  res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value));
}
function controller(t, base, lanes = new Map(), zone = z) {
  const p = Playback.model({ zone, lanes,
    call: async (name, options) => {
      const [method, route] = name === 'state'
        ? ['GET', `/zones/${encodeURIComponent(options.params.room)}/state`] : ['POST', '/seek'];
      assert.ok(name === 'state' || name === 'seek', 'no queue, volume, resume or source replacement call');
      const r = await fetch(base + route, { method, redirect: 'error',
        headers: { Accept: 'application/json', ...(method === 'POST' ? { 'Content-Type': 'application/json' } : {}) },
        body: method === 'POST' ? JSON.stringify(options.body) : undefined,
        signal: AbortSignal.timeout(3000) });
      const data = await r.json();
      if (!r.ok) throw Object.assign(new Error(data.detail || 'HTTP error'), { status: r.status, code: data.code });
      return data;
    } });
  t.after(() => p.dispose());
  const initial = sample(); initial.zone = clone(zone); p.observe(initial); return p;
}

const statePath = '/zones/A%20%26%20B%2F%3F%23%40S1/state';

test('exact-time and native nudges cross real HTTP without resuming paused music or changing its queue', { timeout: 5000 }, async (t) => {
  const calls = [], value = sample();
  const base = await server(t, (req, res) => {
    const entry = [req.method, req.url]; calls.push(entry);
    if (req.method === 'GET') { reply(res, value); return; }
    assert.equal(req.url, '/seek'); assert.equal(req.headers['content-type'], 'application/json');
    let body = ''; req.on('data', (part) => { body += part; });
    req.on('end', () => {
      const b = JSON.parse(body); entry.push(b);
      value.track.position_secs = b.to_secs;
      reply(res, { done: 'Seek accepted', changed: true });
    });
  });
  const p = controller(t, base);
  assert.equal(await p.confirm(p.preview(Playback.parseTime('1:02:03'))), true);
  assert.equal(await p.confirm(p.nudge(-15)), true);
  assert.equal(p.view().elapsed, 3708); assert.equal(p.view().transport, 'paused');
  assert.deepEqual(calls, [
    ['GET', statePath], ['POST', '/seek', { zone: 'A & B/?#@S1', to_secs: 3723 }], ['GET', statePath],
    ['GET', statePath], ['POST', '/seek', { zone: 'A & B/?#@S1', to_secs: 3708 }], ['GET', statePath],
  ]);
  assert.equal(value.track.queue_position, 1); assert.equal(value.track.uri, 'renderer:synthetic');
});

test('a real lost seek acknowledgement stays unresolved after its card disappears and returns', { timeout: 5000 }, async (t) => {
  let commands = 0, reads = 0; const value = sample(), arrived = deferred(), lanes = new Map();
  const base = await server(t, (req, res) => {
    if (req.method === 'GET') { reads++; reply(res, value); return; }
    let text = ''; req.on('data', (part) => { text += part; });
    req.on('end', () => {
      commands++; value.track.position_secs = JSON.parse(text).to_secs;
      arrived.resolve(() => req.socket.destroy());
    });
  });
  const old = controller(t, base, lanes);
  const operation = old.confirm(old.preview(90)); const loseReply = await arrived.promise;
  old.dispose(); const next = controller(t, base, lanes);
  assert.equal(next.view().pending, true); assert.equal(next.preview(120), null);
  assert.equal(await next.refresh(), false); assert.equal(reads, 1);
  loseReply(); assert.equal(await operation, false);
  assert.equal(next.view().uncertain, true); assert.match(next.view().result.detail, /not repeated/);
  next.observe(value); assert.equal(next.view().seekable, false, 'a passive sample does not acknowledge the old write');
  next.dispose(); const returned = controller(t, base, lanes);
  assert.equal(returned.view().uncertain, true);
  assert.equal(await returned.refresh(), true); assert.equal(returned.view().elapsed, 90);
  assert.equal(returned.view().uncertain, false); assert.equal(returned.view().seekable, true);
  assert.equal(commands, 1); assert.equal(reads, 2);
});

test('a slow real HTTP seek keeps replacement-card controls busy until the old response settles', { timeout: 5000 }, async (t) => {
  let commands = 0; const value = sample(), arrived = deferred(), lanes = new Map();
  const base = await server(t, (req, res) => {
    if (req.method === 'GET') { reply(res, value); return; }
    req.resume(); req.on('end', () => {
      commands++; value.track.position_secs = 90;
      arrived.resolve(() => reply(res, { done: 'Seek accepted', changed: true }));
    });
  });
  const old = controller(t, base, lanes), operation = old.confirm(old.preview(90));
  const acknowledge = await arrived.promise; old.dispose(); const next = controller(t, base, lanes);
  assert.equal(next.view().pending, true); assert.equal(next.preview(150), null); assert.equal(commands, 1);
  acknowledge(); assert.equal(await operation, true);
  assert.equal(next.view().pending, false); assert.equal(next.view().fresh, false);
  assert.equal(next.view().result.status, 'accepted'); assert.equal(await next.refresh(), true);
  assert.equal(next.view().elapsed, 90); assert.equal(commands, 1);
});

test('a different queue slot with the same URI in real preflight HTTP prevents the selected seek', { timeout: 5000 }, async (t) => {
  const calls = [], value = sample(); value.track.queue_position = 2;
  const base = await server(t, (req, res) => { calls.push(req.method); reply(res, value); });
  const p = controller(t, base);
  assert.equal(await p.confirm(p.preview(100)), false); assert.deepEqual(calls, ['GET']);
  assert.equal(p.view().result.status, 'not_sent'); assert.match(p.view().result.detail, /changed/);
});

test('real HTTP policy errors retain their detail without replaying a seek or hiding its result on recreation', { timeout: 5000 }, async (t) => {
  let commands = 0; const lanes = new Map();
  const base = await server(t, (req, res) => {
    if (req.method === 'GET') { reply(res, sample()); return; }
    req.resume(); commands++; reply(res, { code: 'POLICY_DENIED', detail: 'Denied by house policy' }, 403);
  });
  const p = controller(t, base, lanes); assert.equal(await p.confirm(p.nudge(15)), false); p.dispose();
  const next = controller(t, base, lanes);
  assert.equal(commands, 1); assert.match(next.view().result.detail, /Denied by house policy/);
  assert.match(next.view().result.detail, /not repeated/); assert.equal(next.preview(150), null);
});

test('a malformed real HTTP success cannot become a seek acknowledgement in a new view', { timeout: 5000 }, async (t) => {
  let commands = 0; const lanes = new Map();
  const base = await server(t, (req, res) => {
    if (req.method === 'GET') { reply(res, sample()); return; }
    commands++; req.resume(); reply(res, { done: 'Missing changed field' });
  });
  const p = controller(t, base, lanes); assert.equal(await p.confirm(p.preview(90)), false); p.dispose();
  const next = controller(t, base, lanes); assert.equal(next.view().uncertain, true);
  assert.match(next.view().result.detail, /incomplete/); assert.equal(next.preview(120), null);
  assert.equal(commands, 1);
});

test('production card registration shares its own seek map and preserves the existing native action gate', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/playback.js'), 'utf8');
  let factory;
  const ctx = vm.createContext({ ROUTES: {}, registerCardPanel: (fn) => { factory = fn; } });
  vm.runInContext(source, ctx);
  assert.deepEqual(clone(ctx.ROUTES), { seek: ['POST', '/seek'] });
  const options = [], seen = [], listeners = new Set(), parts = { zone: z, writing: false }, other = { zone: z };
  const work = () => 'result';
  ctx.capture = (value) => { options.push(value); return { suspend() {}, resume() {}, dispose() {} }; };
  let busy = false; ctx.transportBusy = () => busy;
  ctx.call = () => {}; ctx.el = () => {}; ctx.ownsCard = (v) => v === parts || v === other;
  ctx.act = (p, fn) => { seen.push([p, fn]); return fn(); };
  ctx.document = { hidden: false, addEventListener(_, fn) { listeners.add(fn); }, removeEventListener(_, fn) { listeners.delete(fn); } };
  ctx.window = ctx.document;
  vm.runInContext('PlaybackRemote.panel = capture;', ctx);
  const a = factory(parts), b = factory(other);
  assert.equal(options[0].current(), true); assert.equal(options[0].execute(work), 'result');
  assert.equal(seen[0][0], parts); assert.equal(seen[0][1], work);
  assert.equal(options[0].blocked(), false); parts.writing = true; assert.equal(options[0].blocked(), true);
  parts.writing = false; busy = true; assert.equal(options[0].blocked(), true);
  busy = false; assert.equal(options[0].blocked(), false);
  assert.equal(options[0].externalRefresh, true, 'keep the shared post-action state read');
  assert.equal(options[0].lanes, options[1].lanes, 'all panels in the same remote share ownership');
  assert.ok(options[0].lanes);
  a.dispose(); b.dispose(); assert.equal(listeners.size, 0);
});

test('a lost HTTP acknowledgement cannot be bypassed by electing another coordinator', { timeout: 5000 }, async (t) => {
  const original = { household: 'S2', coordinator_room: 'Alpha', members: ['Alpha', 'Beta'] };
  const successor = { ...original, coordinator_room: 'Beta', members: ['Beta', 'Alpha'] };
  const value = sample(); value.zone = original;
  const arrived = deferred(), lanes = new Map(), calls = [];
  const base = await server(t, (req, res) => {
    calls.push([req.method, req.url]);
    if (req.method === 'GET') { reply(res, value); return; }
    let body = ''; req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => {
      assert.deepEqual(JSON.parse(body), { zone: 'Alpha@S2', to_secs: 90 });
      value.track.position_secs = 90; arrived.resolve(() => res.destroy());
    });
  });
  const old = controller(t, base, lanes, original);
  const pending = old.confirm(old.preview(90)), dropReply = await arrived.promise;
  old.dispose(); value.zone = successor;
  const current = controller(t, base, lanes, successor);
  assert.equal(current.view().pending, true); assert.equal(current.preview(120), null);
  assert.equal(await current.refresh(), false);
  dropReply(); assert.equal(await pending, false);
  current.observe(value); assert.equal(current.view().uncertain, true);
  assert.equal(current.preview(150), null);
  assert.equal(await current.refresh(), true); assert.equal(current.view().elapsed, 90);
  assert.deepEqual(calls, [
    ['GET', '/zones/Alpha%40S2/state'], ['POST', '/seek'], ['GET', '/zones/Beta%40S2/state'],
  ]);
});

test('HTTP read-back of one split group cannot authorize writes to the other', { timeout: 5000 }, async (t) => {
  const original = { household: 'S2', coordinator_room: 'Alpha', members: ['Alpha', 'Beta'] };
  const alpha = { ...original, members: ['Alpha'] };
  const beta = { ...original, coordinator_room: 'Beta', members: ['Beta'] };
  const lanes = new Map(), calls = []; let split = false;
  const base = await server(t, (req, res) => {
    calls.push([req.method, req.url]);
    if (req.method === 'POST') { req.resume(); res.destroy(); return; }
    const value = sample(); value.track.position_secs = 90;
    value.zone = split ? (req.url === '/zones/Beta%40S2/state' ? beta : alpha) : original;
    reply(res, value);
  });
  const old = controller(t, base, lanes, original);
  assert.equal(await old.confirm(old.preview(90)), false); old.dispose(); split = true;
  const a = controller(t, base, lanes, alpha), b = controller(t, base, lanes, beta);
  assert.equal(await a.refresh(), true); assert.equal(a.view().seekable, true);
  assert.equal(b.view().uncertain, true); assert.equal(b.preview(150), null);
  assert.equal(await b.refresh(), true); assert.equal(b.view().seekable, true);
  assert.deepEqual(calls, [
    ['GET', '/zones/Alpha%40S2/state'], ['POST', '/seek'],
    ['GET', '/zones/Alpha%40S2/state'], ['GET', '/zones/Beta%40S2/state'],
  ]);
});
