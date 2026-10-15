'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const R = require('../crates/fsonos-api/src/remote/favorite-routines.js');
const clone = (v) => JSON.parse(JSON.stringify(v));
const zones = () => [{ household: 'S2', coordinator_room: 'Kitchen', members: ['Kitchen', 'Study'] }];
const favorites = () => [{ id: 'FV:2/1', title: 'Morning Music', kind: 'container', description: 'Spotify' }];
const draft = (extra = {}) => ({ label: 'Morning', cadence: 'weekdays', time: '07:30', favorite: 'FV:2/1', ...extra });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const nonce = () => '0123456789abcdef0123456789abcdef';
function backend() {
  const b = { groups: zones(), favorites: favorites(), scenes: [], rows: [], calls: [], override: null };
  b.call = async (name, { body, params, query } = {}) => {
    b.calls.push({ name, body: body && clone(body), params, query });
    if (b.override) { const v = await b.override(name, body, params, query); if (v !== undefined) return v; }
    switch (name) {
      case 'routineZones': return clone(b.groups);
      case 'routineFavorites': assert.equal(query.zone, 'Kitchen@S2'); return clone(b.favorites);
      case 'routineScenes': return clone(b.scenes);
      case 'routineSchedules': return clone(b.rows);
      case 'routineSaveScene': {
        const scene = { name: params.name, saved: 100, ...clone(body) };
        scene.groups[0].source = { kind: 'favorite', name: 'Morning Music', uri: 'x-rincon-cpcontainer:fixture' };
        b.scenes.push(scene); return clone(scene);
      }
      case 'routineAdd': {
        const row = { id: b.rows.length + 1, when: body.when.startsWith('in ') ? 'once 2026-10-16T07:30:00+00:00' : body.when,
          action: 'apply_scene', scene: body.scene, creator: 'loopback-http', enabled: true,
          summary: `Apply ${body.scene}`, next: '2026-10-16T07:30:00+00:00' };
        b.rows.push(row); return clone(row);
      }
      default: throw new Error(`Unexpected route: ${name}`);
    }
  };
  return b;
}
function memory() {
  let value = null;
  return { load: () => clone(value), save: (v) => { value = clone(v); }, clear: () => { value = null; } };
}
async function ready(b = backend(), options = {}) {
  const c = R.model({ call: b.call, nonce, ...options });
  await c.refresh(); await c.choose('Kitchen@S2'); c.review(draft());
  assert.ok(c.state.pending);
  return { b, c, p: c.state.pending };
}
const writes = (b) => b.calls.filter((r) => ['routineSaveScene', 'routineAdd'].includes(r.name));

test('one reviewed scene and one enabled routine; no speaker mutation routes', async () => {
  const { b, c, p } = await ready();
  assert.equal(writes(b).length, 0);
  await c.confirm(p);
  assert.deepEqual(writes(b).map((r) => r.name), ['routineSaveScene', 'routineAdd']);
  assert.equal(c.state.phase, 'complete');
  assert.deepEqual(b.scenes[0].volumes, {}); assert.deepEqual(b.scenes[0].mutes, {});
  assert.deepEqual(b.scenes[0].groups[0].members, ['Study@S2']);
  assert.equal(b.scenes[0].groups[0].coordinator, 'Kitchen@S2');
  assert.equal(writes(b)[0].body.groups[0].source.name, 'FV:2/1');
  assert.equal(writes(b)[1].body.scene, p.name);
  assert.ok([...p.name].length <= 64);
});

test('repeated confirms share no second dispatch, even during preflight', async () => {
  const { b, c, p } = await ready(); const gate = deferred();
  b.override = async (name) => name === 'routineZones' ? gate.promise : undefined;
  const first = c.confirm(p); await c.confirm(p); gate.resolve(zones()); await first; await c.confirm(p);
  assert.equal(writes(b).length, 2);
});

test('review data is a deeply immutable copy', async () => {
  const { c, p } = await ready();
  assert.throws(() => { p.definition.groups[0].members.push('Other@S2'); }, TypeError);
  assert.throws(() => { p.favorite.id = 'FV:2/2'; }, TypeError);
  assert.equal(p.when, 'weekdays 07:30');
});

for (const [name, mutate] of [
  ['regrouping', (b) => { b.groups[0].members = ['Kitchen']; }],
  ['different household', (b) => { b.groups[0].household = 'S1'; }],
  ['favorite removal', (b) => { b.favorites = []; }],
  ['favorite retitling', (b) => { b.favorites[0].title = 'Different'; }],
  ['favorite source-kind change', (b) => { b.favorites[0].kind = 'stream'; }],
  ['scene name collision', (b, p) => { b.scenes = [{ name: p.name.toUpperCase() }]; }],
]) test(`${name} invalidates review before any write`, async () => {
  const { b, c, p } = await ready(); mutate(b, p); await c.confirm(p);
  assert.equal(writes(b).length, 0); assert.equal(c.state.attempt, null); assert.ok(c.state.message);
});

test('close during preflight never writes; a late read cannot revive review', async () => {
  const { b, c, p } = await ready(); const gate = deferred();
  b.override = async (name) => name === 'routineZones' ? gate.promise : undefined;
  const run = c.confirm(p); c.close(); gate.resolve(zones()); await run;
  assert.equal(writes(b).length, 0); assert.equal(c.state.pending, null);
});

test('hidden after scene acceptance stops before scheduling; continuation requires explicit review', async () => {
  let active = true;
  const { b, c, p } = await ready(backend(), { active: () => active });
  b.override = async (name) => { if (name === 'routineSaveScene') active = false; };
  await c.confirm(p); assert.equal(writes(b).length, 1); assert.equal(c.state.phase, 'scene_ready');
  active = true; await c.continue(p); assert.equal(writes(b).length, 1);
  await c.recover(); c.reviewContinue(); await c.continue(c.state.pending);
  assert.equal(writes(b).length, 2); assert.equal(c.state.phase, 'complete');
});

test('closing during the PUT does not lose its result or send the second write', async () => {
  const { b, c, p } = await ready(); const gate = deferred();
  b.override = async (name) => { if (name === 'routineSaveScene') await gate.promise; };
  const run = c.confirm(p);
  while (!writes(b).length) await new Promise(setImmediate);
  c.close(); gate.resolve(); await run;
  assert.equal(c.state.phase, 'scene_ready'); assert.equal(writes(b).length, 1);
});

test('scene edit after its acknowledgement prevents scheduling', async () => {
  const { b, c, p } = await ready(); let reads = 0;
  b.override = async (name) => { if (name === 'routineScenes' && ++reads === 2) b.scenes[0].groups[0].source.uri = 'different'; };
  await c.confirm(p); assert.equal(writes(b).length, 1); assert.equal(c.state.phase, 'unknown');
});

test('mismatched scene acknowledgement never creates a schedule', async () => {
  const { b, c, p } = await ready(); b.override = async (name) => name === 'routineSaveScene' ? { name: p.name } : undefined;
  await c.confirm(p); assert.equal(writes(b).length, 1); assert.equal(c.state.phase, 'unknown');
});

test('lost PUT acknowledgement is observed read-only, then requires new explicit schedule confirmation', async () => {
  const b = backend(), journal = memory(); let fail = true;
  const raw = b.call;
  b.call = async (...args) => { const result = await raw(...args); if (args[0] === 'routineSaveScene' && fail) { fail = false; throw new Error('lost PUT reply'); } return result; };
  const { c, p } = await ready(b, { journal }); await c.confirm(p);
  assert.equal(writes(b).length, 1);
  const reloaded = R.model({ call: b.call, nonce, journal }); await reloaded.recover();
  assert.equal(writes(b).length, 1); assert.equal(reloaded.state.phase, 'scene_ready');
  reloaded.reviewContinue(); await reloaded.continue(reloaded.state.pending);
  assert.equal(writes(b).length, 2); assert.equal(reloaded.state.phase, 'complete');
});

test('lost POST reply stays single-dispatch across reload and repeated recovery', async () => {
  const b = backend(), journal = memory(), raw = b.call;
  b.call = async (...args) => { const result = await raw(...args); if (args[0] === 'routineAdd') throw new Error('lost POST reply'); return result; };
  const { c, p } = await ready(b, { journal }); await c.confirm(p);
  assert.equal(c.state.phase, 'unknown');
  const reload = R.model({ call: b.call, nonce, journal }); await reload.recover(); await reload.recover();
  assert.equal(reload.state.phase, 'complete'); assert.match(reload.state.message, /observation/);
  reload.reviewContinue(); await reload.continue(p); await reload.confirm(p);
  assert.equal(writes(b).length, 2);
});

test('absent schedule after a sent POST is not permission to retry', async () => {
  const { b, c, p } = await ready(); b.override = async (name) => { if (name === 'routineAdd') throw new Error('timeout'); };
  await c.confirm(p); await c.recover(); c.reviewContinue(); await c.continue(p);
  assert.equal(c.state.phase, 'unknown'); assert.equal(writes(b).length, 2);
  assert.match(c.state.message, /may still finish/);
});

test('malformed successful POST is uncertain, not a second submission', async () => {
  const { b, c, p } = await ready(); b.override = async (name) => name === 'routineAdd' ? {} : undefined;
  await c.confirm(p); await c.confirm(p); assert.equal(writes(b).length, 2); assert.equal(c.state.phase, 'unknown');
});

test('an existing routine referencing the private scene blocks another add', async () => {
  const { b, c, p } = await ready(); b.rows = [{ id: 9, action: 'apply_scene', scene: p.name }];
  await c.confirm(p); assert.equal(writes(b).length, 1);
});

test('duplicate schedules or edited definitions cannot be used as recovery evidence', async () => {
  const { b, c, p } = await ready(); await c.confirm(p);
  b.rows.push({ ...b.rows[0], id: 2 }); await c.recover(); assert.equal(c.state.phase, 'unknown');
  b.rows.pop(); b.rows[0].when = 'daily 09:00'; await c.recover(); assert.equal(c.state.phase, 'unknown');
  assert.equal(writes(b).length, 2);
});

test('journal failure before PUT sends no write', async () => {
  const journal = { load: () => null, save: () => { throw new Error('storage blocked'); } };
  const { b, c, p } = await ready(backend(), { journal }); await c.confirm(p);
  assert.equal(writes(b).length, 0); assert.match(c.state.message, /storage blocked/);
});

test('journal failure before POST never sends it', async () => {
  const journal = memory(), save = journal.save; journal.save = (v) => { if (v.scheduleSent) throw new Error('quota'); save(v); };
  const { b, c, p } = await ready(backend(), { journal }); await c.confirm(p);
  assert.equal(writes(b).length, 1); assert.match(c.state.message, /quota/);
});

test('corrupt recovery record blocks new creation', async () => {
  const b = backend(), c = R.model({ call: b.call, journal: { load: () => ({ version: 5 }) } });
  await c.refresh(); c.review(draft()); await c.confirm({}); assert.equal(writes(b).length, 0); assert.equal(c.state.phase, 'blocked');
});

test('view callback failure cannot lose acknowledgement or trigger a replay', async () => {
  const { b, c, p } = await ready(backend(), { changed: () => { throw new Error('render'); } });
  await c.confirm(p); assert.equal(c.state.phase, 'complete'); assert.equal(writes(b).length, 2);
});

test('explicit new routine clears only a confirmed attempt', async () => {
  const journal = memory(), { b, c, p } = await ready(backend(), { journal });
  await c.confirm(p); assert.ok(journal.load()); await c.newRoutine(); assert.equal(journal.load(), null);
  assert.equal(c.state.phase, 'choosing'); assert.equal(writes(b).length, 2);
});

test('timing and malformed choice bounds fail closed', () => {
  assert.equal(R.whenOf(draft()), 'weekdays 07:30');
  assert.equal(R.whenOf(draft({ cadence: 'delay', minutes: '45' })), 'in 45m');
  for (const d of [draft({ time: '25:00' }), draft({ cadence: 'delay', minutes: '1.5' }), draft({ cadence: 'delay', minutes: '10081' })]) assert.throws(() => R.whenOf(d));
  assert.throws(() => R.groupsOf([{ ...zones()[0], members: ['Kitchen', 'Kitchen'] }]));
  assert.throws(() => R.favoritesOf([...favorites(), ...favorites()]));
  assert.deepEqual(R.favoritesOf([{ ...favorites()[0], kind: 'unplayable' }]), []);
});

test('real loopback HTTP executes the exact PUT and POST contract', async (t) => {
  const b = backend(), received = [];
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost'); let raw = ''; for await (const chunk of req) raw += chunk;
    const path = url.pathname, method = req.method, body = raw ? JSON.parse(raw) : undefined;
    received.push([method, path]);
    let result;
    if (path === '/zones') result = await b.call('routineZones');
    else if (path === '/favorites') result = await b.call('routineFavorites', { query: { zone: url.searchParams.get('zone') } });
    else if (path === '/scenes') result = await b.call('routineScenes');
    else if (path.startsWith('/scenes/') && method === 'PUT') result = await b.call('routineSaveScene', { body, params: { name: decodeURIComponent(path.slice(8)) } });
    else if (path === '/schedules') result = await b.call(method === 'POST' ? 'routineAdd' : 'routineSchedules', { body });
    else { res.writeHead(404); return res.end(); }
    res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(result));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve)); t.after(() => server.close());
  const call = async (name, { body, params = {}, query } = {}) => {
    const [method, template] = R.routes[name]; const url = new URL(template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key])), `http://127.0.0.1:${server.address().port}`);
    if (query) url.search = new URLSearchParams(query);
    const response = await fetch(url, { method, headers: { 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
    assert.equal(response.status, 200); return response.json();
  };
  const { c, p } = await ready(b, { call }); await c.confirm(p);
  assert.equal(c.state.phase, 'complete');
  assert.equal(received.filter(([m]) => m === 'POST').length, 1);
  assert.equal(received.filter(([m]) => m === 'PUT').length, 1);
});
