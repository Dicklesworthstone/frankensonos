'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const V = require('../crates/fsonos-api/src/remote/dashboard-visualizer.js');
const z = (changes = {}) => ({ household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen'], transport_state: 'playing', ...changes });
const p = (zone = z(), changes = {}) => ({ zone, transport_state: 'playing', volume: 20, group_volume: 22,
  track: { uri: 'synthetic:track', title: 'Synthetic track', position_secs: 30, duration_secs: 180, queue_position: 1 }, ...changes });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
const tick = () => new Promise((r) => setImmediate(r));
function fixture(options = {}) {
  let inventory = [z()], position = p(), hook = null, now = 1000;
  const calls = [];
  const model = C.model({ now: () => now, ...options, call: async (url, body) => {
    calls.push({ url, body });
    if (hook) return hook(url, body);
    if (url === '/zones') return structuredClone(inventory);
    if (body !== undefined) return { done: 'Accepted', changed: true };
    return structuredClone(position);
  } });
  return { model, calls, setZones(v) { inventory = v; }, setPlayback(v) { position = v; },
    hook(v) { hook = v; }, time(v) { now = v; }, writes: () => calls.filter((c) => c.body !== undefined),
    async ready() { assert.equal(await model.refresh(), true); return model; } };
}
test('qualified groups are immutable and same-name rooms in separate households stay distinct', () => {
  const rows = C.zones([z(), z({ household: 'S1' })]);
  assert.deepEqual(rows.map(C.key), ['Studio@S2', 'Studio@S1']);
  assert.throws(() => rows[0].members.push('Uninvited'));
});
test('invalid topology, duplicate members and ambiguous target encodings are rejected', () => {
  for (const input of [null, {}, [null], [z({ members: [] })], [z({ members: ['Studio', 'Studio'] })],
    [z(), z()], [z({ transport_state: 'invalid' })], [z({ degraded: ['Missing'] })],
    [z({ household: 'b@c', coordinator_room: 'a', members: ['a'] }), z({ household: 'c', coordinator_room: 'a@b', members: ['a@b'] })]]) {
    assert.throws(() => C.zones(input));
  }
});
test('playback must belong to the exact selected group and carry valid timing and volume', () => {
  for (const bad of [null, p(z({ household: 'S1' })), p(z({ members: ['Studio'] })), p(z(), { volume: 101 }),
    p(z(), { transport_state: 'invalid' }), p(z(), { track: { uri: 'x', queue_position: 0 } }),
    p(z(), { track: { uri: 'x', duration_secs: 20, position_secs: 21 } })]) assert.throws(() => C.playback(bad, z()));
  assert.equal(C.playback(p(z(), { track: null }), z()).track, null);
});
test('estimated clocks advance only fresh playing samples and never beyond a known duration', () => {
  const item = { state: p(), at: 1000, fresh: true };
  assert.equal(C.elapsed(item, 4000), 33); assert.equal(C.elapsed(item, 999999), 180);
  item.fresh = false; assert.equal(C.elapsed(item, 4000), 30);
  item.fresh = true; item.state.transport_state = 'paused'; assert.equal(C.elapsed(item, 4000), 30);
  assert.equal(C.elapsed({ state: { track: { uri: 'stream' } } }, 5000), null);
  assert.equal(C.clock(3661), '1:01:01'); assert.equal(C.clock(null), '—:—');
});
test('initial reading selects a playing group but never invents an empty house', async () => {
  const f = fixture(), m = await f.ready(); assert.equal(m.state.selected, 'Studio@S2'); assert.equal(m.canControl(), true);
  f.setZones([]); assert.equal(await m.refresh(), true); assert.equal(m.state.selected, ''); assert.equal(m.canControl(), false);
  assert.equal(await m.command('resume'), false); assert.equal(f.writes().length, 0);
});
test('removed selected rooms are not silently replaced as a control target', async () => {
  const f = fixture(), m = await f.ready(); const other = z({ household: 'S1' }); f.setZones([other]); f.setPlayback(p(other));
  await m.refresh(); assert.equal(m.state.selected, ''); assert.equal(m.canControl(), false);
  assert.equal(m.select('Studio@S1'), true); assert.equal(m.canControl(), true);
});
test('state reads are capped at four concurrent requests', async () => {
  let active = 0, peak = 0;
  const rows = Array.from({ length: 15 }, (_, i) => z({ household: `synthetic-${i}` }));
  const model = C.model({ call: async (url) => {
    if (url === '/zones') return rows;
    active++; peak = Math.max(peak, active); await tick(); active--;
    return p(rows.find((zone) => url.includes(encodeURIComponent(C.key(zone)))));
  } });
  await model.refresh(); assert.equal(peak, 4); assert.equal(model.state.items.length, 15);
});
test('failed room reads preserve stale observations without presenting them as current', async () => {
  const f = fixture(), m = await f.ready(); f.hook((url) => { if (url === '/zones') return [z()]; throw new Error('Offline'); });
  assert.equal(await m.refresh(), false); assert.equal(m.selected().fresh, false); assert.equal(m.selected().state.track.title, 'Synthetic track');
  assert.equal(m.canControl(), false);
});
test('pending refreshes are not duplicated and hidden results cannot repopulate state', async () => {
  const f = fixture(), d = deferred(); f.hook(() => d.promise);
  const first = f.model.refresh(); assert.equal(await f.model.refresh(), false); f.model.suspend();
  d.resolve([z()]); assert.equal(await first, false); assert.equal(f.model.state.items.length, 0);
  assert.equal(f.calls.length, 1);
});
test('controls expire when state has not been observed for thirty seconds', async () => {
  const f = fixture(), m = await f.ready(); f.time(31001); assert.equal(m.canControl(), false);
  assert.equal(await m.command('next'), false); assert.equal(f.writes().length, 0);
});
test('next performs an exact-group preflight and sends one native command without source changes', async () => {
  const f = fixture(), m = await f.ready(); assert.equal(await m.command('next'), true);
  assert.deepEqual(f.writes(), [{ url: '/next', body: { zone: 'Studio@S2' } }]);
});
test('group volume steps use native policy and ignore unexpected input properties', async () => {
  const f = fixture(), m = await f.ready();
  assert.equal(await m.command('volume', { delta: 200 }), false);
  assert.equal(await m.command('volume', { delta: -5, source_uri: 'untrusted' }), true);
  assert.deepEqual(f.writes()[0], { url: '/volume', body: { zone: 'Studio@S2', delta: -5, group: true } });
});
test('a changed track or transport state consumes the click without sending it elsewhere', async () => {
  for (const change of [{ transport_state: 'paused' }, { track: { ...p().track, queue_position: 2 } }]) {
    const f = fixture(), m = await f.ready(); f.setPlayback(p(z(), change));
    assert.equal(await m.command('next'), false); assert.equal(f.writes().length, 0); assert.match(m.state.message, /No command was sent/);
  }
});
test('double submit, target changes and refresh cannot interleave with a pending command', async () => {
  const f = fixture(), m = await f.ready(), d = deferred();
  f.hook((url, body) => body ? d.promise : p());
  const first = m.command('next'); await tick();
  assert.equal(await m.command('next'), false); assert.equal(m.select('Studio@S2'), false); assert.equal(await m.refresh(), false);
  assert.equal(f.writes().length, 1); d.resolve({ done: 'Accepted', changed: true }); await first;
});
test('hiding during preflight prevents dispatch even if the read later succeeds', async () => {
  const f = fixture(), m = await f.ready(), d = deferred(); f.hook(() => d.promise);
  const work = m.command('next'); m.suspend(); d.resolve(p()); assert.equal(await work, false);
  assert.equal(f.writes().length, 0);
});
test('lost or malformed write results remain uncertain across passive reads until explicit refresh', async () => {
  for (const result of [null, {}, { done: 'ok', changed: 'yes' }]) {
    const f = fixture(), m = await f.ready(); f.hook((url, body) => body ? result : p());
    assert.equal(await m.command('next'), false); assert.equal(m.state.uncertain, true); assert.equal(f.writes().length, 1);
    f.hook(null); await m.refresh(); assert.equal(m.canControl(), false);
    assert.equal(await m.command('next'), false); assert.equal(f.writes().length, 1);
    await m.refresh(true); assert.equal(m.state.uncertain, false); assert.equal(m.canControl(), true);
  }
});
test('server refusal retains actionable policy text and is never retried', async () => {
  const f = fixture(), m = await f.ready();
  f.hook((url, body) => { if (!body) return p(); throw Object.assign(new Error('Policy denied'), { hint: 'Ask the owner.' }); });
  await m.command('resume'); assert.equal(f.writes().length, 1); assert.match(m.state.message, /Ask the owner/);
});
test('design-preview mode cannot issue any command', async () => {
  const f = fixture({ demo: true }), m = await f.ready();
  for (const verb of ['next', 'previous', 'pause', 'resume', 'volume']) assert.equal(await m.command(verb, { delta: 5 }), false);
  assert.equal(f.writes().length, 0);
});
test('disposal prevents all subsequent reads and writes', async () => {
  const f = fixture(), m = await f.ready(); m.dispose(); const count = f.calls.length; m.resume();
  assert.equal(await m.refresh(), false); assert.equal(await m.command('next'), false); assert.equal(f.calls.length, count);
});
test('request deadline covers an indefinitely stalled response body', async () => {
  let timeout, cleared = false;
  const req = C.request({ fetcher: async () => ({ ok: true, text: () => new Promise(() => {}) }),
    schedule: (fn) => { timeout = fn; return 1; }, cancel: () => { cleared = true; } });
  const waiting = req('/zones'); await tick(); timeout(); await assert.rejects(waiting, /did not answer/); assert.equal(cleared, true);
});
test('the HTTP client uses same-origin JSON, refuses redirects and never repeats POST', async () => {
  const calls = [];
  const req = C.request({ fetcher: async (url, init) => { calls.push({ url, init }); throw new Error('Lost reply'); } });
  await assert.rejects(req('/next', { zone: 'Studio@S2' }), /Lost reply/);
  assert.equal(calls.length, 1); assert.equal(calls[0].init.redirect, 'error'); assert.equal(calls[0].init.credentials, 'same-origin');
  assert.equal(calls[0].init.headers['Content-Type'], 'application/json');
});
test('visualizer backing stores are bounded even on high-DPI displays', () => {
  for (const [w, h, dpr] of [[400, 500, 3], [3840, 2160, 4], [1, 1, 1]]) {
    const [x, y] = V.dimensions(w, h, dpr); assert.ok(x * y <= 1803000); assert.ok(x >= 1 && y >= 1);
  }
});
test('a missing canvas context degrades without throwing or scheduling animations', () => {
  const renderer = V.create({ getContext: () => null }, { raf: () => { throw new Error('must not animate'); }, caf: () => {}, now: () => 0 });
  renderer.resize(100, 100, 2); renderer.visible(true); renderer.mode('contour'); renderer.track(1, true); renderer.dispose();
  assert.equal(renderer.supported, false);
});
