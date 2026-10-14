'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { once } = require('node:events');
const dj = require('../crates/fsonos-api/src/remote/dj-steering.js');

const groups = () => [
  { household: 'S1', coordinator_room: 'Kitchen', members: ['Kitchen', 'Dining'] },
  { household: 'S2', coordinator_room: 'Kitchen', members: ['Kitchen'] },
];
const moods = () => ({ moods: [{ name: 'focus', constraints: { include_genres: ['classical'] } },
  { name: 'calm', constraints: { energy_bias: -1 } }], programs: [], now: { source: 'none' } });
const steering = () => ({ source: 'session', mood: 'focus',
  constraints: { include_genres: ['classical'], exclude_composers: ['Wagner'], energy_bias: 0, allow_long_works: false },
  added: { exclude_composers: ['Wagner'], energy_bias: 0, allow_long_works: false },
  expires_at: 2000000000, expires_in_secs: 7200 });
const work = () => ({ composer: 'Brahms', title: 'Symphony No. 4', movements: 4, movement: 2,
  movement_title: 'Andante moderato', performers: ['Sim Ensemble'], minutes: 40, album: 'Symphonies',
  reason: { summary: 'Balancing the library.', factors: [{ factor: 'energy_fit', weight: 1250 }], relaxed: ['keyword'] } });
const status = () => ({ zone: 'Kitchen', running: true, now: work(), next: [work()], steering: steering(), problem: 'Last top-up failed; will retry.' });
const saved = () => ({ done: 'Steering saved.', changed: true, notes: [] });
const draft = () => ({ mood: 'calm', minutes: '90', constraints: { exclude_artists: ['Earth, Wind & Fire'], energy_bias: -1 } });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const tick = () => new Promise((resolve) => setImmediate(resolve));
function fixture() {
  const calls = [], replies = { djSteeringZones: [], djSteeringMoods: [], djSteeringStatus: [], djSteeringWrite: [] };
  const defaults = { djSteeringZones: groups, djSteeringMoods: moods, djSteeringStatus: status, djSteeringWrite: saved };
  const editor = dj.model({ call: async (name, args) => {
    calls.push([name, args]); assert.ok(Object.hasOwn(replies, name), `Unexpected route: ${name}`);
    if (!replies[name].length) return defaults[name]();
    const reply = replies[name].shift(); if (reply instanceof Error) throw reply; return reply;
  } });
  const open = async (target = 'Kitchen@S1') => { assert.equal(await editor.load(), true); assert.equal(await editor.select(target), true); };
  const writes = () => calls.filter(([name]) => name === 'djSteeringWrite');
  return { editor, calls, replies, open, writes };
}

test('routes reuse the public steering API and cannot start, skip or replace music', () => {
  assert.deepEqual(dj.routes, { djSteeringZones: ['GET', '/zones'], djSteeringMoods: ['GET', '/dj/moods'],
    djSteeringStatus: ['GET', '/zones/{room}/dj'], djSteeringWrite: ['POST', '/dj/steer'] });
});

test('all sixteen constraints round-trip canonically without splitting artist names at commas', () => {
  const value = { include_composers: [' Bach '], exclude_composers: ['Wagner'],
    include_artists: ['Earth, Wind & Fire'], exclude_artists: ['Example'], include_genres: ['jazz'], exclude_genres: ['metal'],
    periods: ['Late Romantic'], exclude_periods: ['MODERN'], decades: [1960], exclude_decades: [1980],
    include_keywords: ['piano'], exclude_keywords: ['vocal'], min_work_minutes: 2, max_work_minutes: 60,
    energy_bias: -2, allow_long_works: true };
  const checked = dj.constraintsOf(value);
  assert.equal(Object.keys(checked).length, 16);
  assert.deepEqual(checked.include_artists, ['Earth, Wind & Fire']);
  assert.deepEqual(checked.include_composers, ['Bach']);
  assert.deepEqual(checked.periods, ['late_romantic']);
  assert.deepEqual(checked.exclude_periods, ['modern']);
  assert.deepEqual(dj.constraintsOf(checked), checked);
  value.include_artists.push('Later');
  assert.equal(checked.include_artists.length, 1);
  assert.throws(() => checked.periods.push('baroque'));
  assert.throws(() => { checked.energy_bias = 0; });
});

test('constraint defaults are sparse and unknown fields cannot be silently erased', () => {
  assert.deepEqual(dj.constraintsOf(), {});
  assert.deepEqual(dj.constraintsOf({ energy_bias: 0, allow_long_works: false, include_artists: [], min_work_minutes: null }), {});
  for (const v of [null, [], { new_filter: ['x'] }, { energy_bias: '1' }, { energy_bias: null },
    { energy_bias: 3 }, { energy_bias: -3 }, { energy_bias: 1.1 }, { allow_long_works: 'false' }]) assert.throws(() => dj.constraintsOf(v));
});

test('every list enforces 32 entries and string bounds count Unicode scalar characters', () => {
  for (const key of Object.keys(dj.lists)) {
    const entry = key.includes('decades') ? 1960 : key.includes('periods') ? 'baroque' : '🎵'.repeat(128);
    assert.equal(dj.constraintsOf({ [key]: Array(32).fill(entry) })[key].length, 32);
    assert.throws(() => dj.constraintsOf({ [key]: Array(33).fill(entry) }));
    for (const value of [null, 'not a list', [null], ['']]) assert.throws(() => dj.constraintsOf({ [key]: value }));
  }
  assert.throws(() => dj.constraintsOf({ include_artists: ['🎵'.repeat(129)] }));
});

test('periods and decades obey the daemon schema rather than fuzzy guesses', () => {
  for (const period of ['medieval', 'renaissance', 'baroque', 'classical', 'romantic', 'late_romantic', 'impressionist', 'modern', 'contemporary']) {
    assert.deepEqual(dj.constraintsOf({ periods: [period] }).periods, [period]);
  }
  assert.deepEqual(dj.constraintsOf({ periods: ['late-romantic'] }).periods, ['late_romantic']);
  assert.throws(() => dj.constraintsOf({ periods: ['80s'] }));
  assert.deepEqual(dj.constraintsOf({ decades: [1000, 65530] }).decades, [1000, 65530]);
  for (const n of [990, 1965, 65540, 1960.1, '1960']) assert.throws(() => dj.constraintsOf({ decades: [n] }));
});

test('length and energy extremes are valid but inverted ranges are rejected', () => {
  for (const energy_bias of [-2, -1, 0, 1, 2]) {
    const c = dj.constraintsOf({ min_work_minutes: 1, max_work_minutes: 600, energy_bias });
    assert.equal(c.energy_bias || 0, energy_bias);
  }
  for (const [min, max] of [[0, 600], [1, 601], [100, 99], [1.5, 2], ['1', 2]]) {
    assert.throws(() => dj.constraintsOf({ min_work_minutes: min, max_work_minutes: max }));
  }
});

test('steering request bounds, empty changes and unknown moods fail before dispatch', () => {
  const base = { mood: 'focus', constraints: {} };
  assert.equal(dj.requestOf('Kitchen@S1', { ...base, minutes: '1' }, ['focus']).for_secs, 60);
  assert.equal(dj.requestOf('Kitchen@S1', { ...base, minutes: '10080' }, ['focus']).for_secs, 604800);
  assert.ok(!Object.hasOwn(dj.requestOf('Kitchen@S1', base, ['focus']), 'for_secs'));
  for (const minutes of ['0', '10081', '-1', '1.5', '1e2', ' ', 60]) assert.throws(() => dj.requestOf('Kitchen@S1', { ...base, minutes }, ['focus']));
  for (const d of [{}, { constraints: { energy_bias: 0 } }, { mood: 'unknown' }, { clear: 'true' }]) assert.throws(() => dj.requestOf('Kitchen@S1', d, ['focus']));
  assert.throws(() => dj.requestOf('x'.repeat(129), base, ['focus']));
  assert.equal(dj.requestOf('🎵'.repeat(128), base, ['focus']).zone, '🎵'.repeat(128));
});

test('clear is only zone and clear, never stale draft filters or a timer', () => {
  assert.deepEqual(dj.requestOf('Kitchen@S1', { ...draft(), clear: true }, []), { zone: 'Kitchen@S1', clear: true });
});

test('topology keeps same-named households distinct and rejects missing or overlapping membership', () => {
  const zones = dj.zonesOf(groups());
  assert.deepEqual(zones.map((z) => z.target), ['Kitchen@S1', 'Kitchen@S2']);
  const reordered = groups(); reordered[0].members.reverse();
  assert.equal(dj.zonesOf(reordered)[0].signature, zones[0].signature);
  for (const bad of [null, {}, [groups()[0], groups()[0]], [{ ...groups()[0], members: ['Dining'] }],
    [{ ...groups()[0], members: [] }], [{ ...groups()[0], members: ['Kitchen', 'Kitchen'] }],
    [{ ...groups()[0], household: '' }], [...groups(), { household: 'S1', coordinator_room: 'Dining', members: ['Dining'] }]]) {
    assert.throws(() => dj.zonesOf(bad));
  }
  assert.deepEqual(dj.zonesOf([]), []);
});

test('reason snapshots retain movement, exact factor multipliers and actual relaxed filters', () => {
  const raw = work(); raw.title = '<img src=x onerror=alert(1)>'; raw.reason.factors.push({ factor: 'penalty', weight: 250 });
  const w = dj.workOf(raw);
  raw.reason.relaxed.push('later');
  assert.equal(w.title, '<img src=x onerror=alert(1)>');
  assert.equal(w.movement, 2); assert.equal(w.movements, 4);
  assert.deepEqual(w.reason.relaxed, ['keyword']);
  assert.deepEqual(w.reason.factors.map((f) => f.weight), [1250, 250]);
  assert.throws(() => { w.reason.factors[0].weight = 1000; });
  assert.equal(dj.workOf({ composer: 'A', title: 'Song', movements: 1, reason: { summary: 'Chosen.' } }).movement, null);
});

test('malformed statuses and explanations do not become an editable stopped DJ', () => {
  const z = dj.zonesOf(groups())[0];
  for (const bad of [null, {}, { ...status(), zone: 'Bedroom' }, { ...status(), running: 'false' },
    { ...status(), next: [work(), work(), work()] }, { ...status(), steering: {} },
    { ...status(), problem: 5 }, { ...status(), steering: { source: 'future' } }]) assert.throws(() => dj.statusOf(bad, z));
  for (const bad of [{ ...work(), movement: 5 }, { ...work(), performers: 'Artist' },
    { ...work(), reason: { summary: 'x', factors: [{ factor: 'fit', weight: 1.5 }] } },
    { ...work(), reason: { summary: 'x', relaxed: [5] } }]) assert.throws(() => dj.workOf(bad));
  assert.deepEqual(dj.statusOf({ zone: 'Kitchen', running: false, steering: { source: 'none' } }, z).next, []);
  for (const value of [{ source: 'session', expires_at: Number.MAX_SAFE_INTEGER }, { source: 'session', expires_in_secs: -1 }]) assert.throws(() => dj.steeringOf(value));
  for (const value of [null, {}, { moods: [{ name: '' }] }, { moods: [{ name: 'calm' }, { name: 'calm' }] }]) assert.throws(() => dj.moodsOf(value));
});

test('model is lazy and requires explicit household-qualified selection', async () => {
  const { editor, calls } = fixture();
  assert.equal(calls.length, 0); assert.equal(editor.review(draft()), null);
  await editor.load();
  assert.equal(editor.state.target, ''); assert.equal(editor.state.ready, false);
  assert.equal(await editor.select('Kitchen'), false);
  await editor.select('Kitchen@S2');
  assert.deepEqual(calls[2], ['djSteeringStatus', { params: { room: 'Kitchen@S2' } }]);
  assert.equal(editor.state.status.problem, 'Last top-up failed; will retry.');
});

test('one reviewed steer rechecks group and session then sends the exact body once', async () => {
  const f = fixture(); await f.open();
  const pending = f.editor.review(draft());
  assert.ok(Object.isFrozen(pending) && Object.isFrozen(pending.body.constraints));
  assert.equal(f.writes().length, 0);
  assert.equal(await f.editor.confirm(pending), true);
  assert.deepEqual(f.calls.slice(3).map(([name]) => name), ['djSteeringZones', 'djSteeringStatus', 'djSteeringWrite', 'djSteeringStatus']);
  assert.deepEqual(f.writes()[0][1].body, { zone: 'Kitchen@S1', mood: 'calm',
    constraints: { exclude_artists: ['Earth, Wind & Fire'], energy_bias: -1 }, for_secs: 5400 });
  assert.equal(f.editor.state.ready, true);
});

test('clear uses its own reviewed request without changing standing preferences', async () => {
  const f = fixture(); await f.open();
  await f.editor.confirm(f.editor.review({ clear: true }));
  assert.deepEqual(f.writes()[0][1], { body: { zone: 'Kitchen@S1', clear: true } });
});

test('canceled, superseded, copied and refreshed reviews cannot be revived', async () => {
  const f = fixture(); await f.open();
  const old = f.editor.review(draft());
  const latest = f.editor.review({ clear: true });
  assert.equal(await f.editor.confirm(old), false);
  assert.equal(await f.editor.confirm({ ...latest }), false);
  f.editor.cancel(); assert.equal(await f.editor.confirm(latest), false);
  const beforeRefresh = f.editor.review(draft()); await f.editor.load();
  assert.equal(await f.editor.confirm(beforeRefresh), false);
  assert.equal(f.writes().length, 0);
});

test('topology preflight rejects changed groups before reading or saving their steering', async () => {
  for (const mutate of [(z) => { z[0].members.push('Hall'); }, (z) => { z[0].coordinator_room = 'Dining'; }, (z) => { z.shift(); }]) {
    const f = fixture(); await f.open(); const newer = groups(); mutate(newer); f.replies.djSteeringZones.push(newer);
    assert.equal(await f.editor.confirm(f.editor.review(draft())), false);
    assert.equal(f.writes().length, 0); assert.equal(f.editor.state.ready, false);
    assert.match(f.editor.state.message, /group changed/);
  }
});

test('preflight rejects changed or expired sessions even when the group is unchanged', async () => {
  for (const mutate of [(s) => { s.mood = 'calm'; }, (s) => { s.added = { include_artists: ['Example'] }; },
    (s) => { s.constraints = { energy_bias: 2 }; }, (s) => { s.expires_at++; }, (s) => { s.source = 'program'; }]) {
    const f = fixture(); await f.open(); const newer = status(); mutate(newer.steering); f.replies.djSteeringStatus.push(newer);
    assert.equal(await f.editor.confirm(f.editor.review(draft())), false);
    assert.equal(f.writes().length, 0); assert.match(f.editor.state.message, /changed or expired/);
  }
});

test('member ordering, advancing tracks and a decreasing countdown do not invalidate steering', async () => {
  const f = fixture(); await f.open();
  const newer = status(); newer.now.movement = 3; newer.steering.expires_in_secs -= 60;
  const zones = groups(); zones[0].members.reverse();
  f.replies.djSteeringZones.push(zones); f.replies.djSteeringStatus.push(newer);
  assert.equal(await f.editor.confirm(f.editor.review(draft())), true);
  assert.equal(f.writes().length, 1);
});

test('double confirmation and other edits cannot overlap a pending preflight', async () => {
  const f = fixture(); await f.open(); const wait = deferred(); f.replies.djSteeringZones.push(wait.promise);
  const pending = f.editor.review(draft()), saving = f.editor.confirm(pending);
  assert.equal(f.editor.state.busy, true);
  assert.equal(await f.editor.confirm(pending), false);
  assert.equal(await f.editor.load(), false);
  assert.equal(await f.editor.select('Kitchen@S2'), false);
  assert.equal(f.editor.review({ clear: true }), null);
  wait.resolve(groups()); assert.equal(await saving, true);
  assert.equal(f.writes().length, 1);
});

test('closing, disposing or closing then reopening during preflight cancels the unsent save', async () => {
  for (const action of ['close', 'dispose', 'reopen']) {
    const f = fixture(); await f.open(); const wait = deferred(); f.replies.djSteeringStatus.push(wait.promise);
    const saving = f.editor.confirm(f.editor.review(draft())); await tick();
    if (action === 'dispose') f.editor.dispose(); else f.editor.close();
    if (action === 'reopen') await f.editor.load();
    wait.resolve(status()); assert.equal(await saving, false);
    assert.equal(f.writes().length, 0); assert.equal(f.editor.state.busy, false);
    if (action !== 'dispose') assert.match(f.editor.state.message, /canceled before saving/);
  }
});

test('closing after dispatch cannot abort, replay or lose ownership of an in-flight save', async () => {
  const f = fixture(); await f.open(); const wait = deferred(); f.replies.djSteeringWrite.push(wait.promise);
  const pending = f.editor.review(draft()), saving = f.editor.confirm(pending); await tick();
  assert.equal(f.writes().length, 1); f.editor.close(); await f.editor.load();
  assert.equal(f.editor.state.busy, true); assert.equal(await f.editor.confirm(pending), false);
  const count = f.calls.length; wait.resolve(saved()); assert.equal(await saving, true);
  assert.equal(f.calls.length, count); assert.equal(f.editor.state.ready, false);
  assert.equal(f.editor.state.message, 'Steering saved.');
  await f.editor.load(); assert.equal(f.editor.state.ready, true);
});

test('failed preflight and malformed status retain the remedy and send no mutation', async () => {
  for (const reply of [new Error('Disconnected.'), {}, { ...status(), zone: 'Bedroom' }]) {
    const f = fixture(); await f.open(); f.replies.djSteeringStatus.push(reply);
    assert.equal(await f.editor.confirm(f.editor.review(draft())), false);
    assert.equal(f.writes().length, 0); assert.equal(f.editor.state.ready, false);
  }
});

test('lost or malformed save replies are terminal until an explicit fresh read', async () => {
  for (const reply of [new Error('Lost reply.'), null, {}, { done: 'Saved.', changed: 'true' },
    { done: 'Saved.', changed: true, notes: [{ message: 'Wrong field.' }] }]) {
    const f = fixture(); await f.open(); f.replies.djSteeringWrite.push(reply);
    const pending = f.editor.review(draft());
    assert.equal(await f.editor.confirm(pending), false);
    assert.equal(await f.editor.confirm(pending), false);
    assert.equal(f.editor.review(draft()), null);
    assert.equal(f.writes().length, 1); assert.match(f.editor.state.message, /Outcome uncertain.*do not resend/);
    assert.equal(f.editor.state.ready, false); await f.editor.load(); assert.equal(f.editor.state.ready, true);
  }
});

test('server note detail survives successful save and fresh-status failure without replay', async () => {
  const f = fixture(); await f.open();
  f.replies.djSteeringWrite.push({ done: 'Stored steering.', changed: true, notes: [{ code: 'POLICY_NOTE', detail: 'An important server detail.' }] });
  f.replies.djSteeringStatus.push(status(), Object.assign(new Error('Status unavailable.'), { hint: 'Check the daemon.' }));
  assert.equal(await f.editor.confirm(f.editor.review(draft())), true);
  assert.match(f.editor.state.message, /Stored steering.*important server detail.*Check the daemon.*do not resend/);
  assert.equal(f.editor.state.ready, false); assert.equal(f.writes().length, 1);
});

test('a changed false acknowledgment is valid and notes are displayed from detail', async () => {
  const f = fixture(); await f.open(); f.replies.djSteeringWrite.push({ done: 'Already cleared.', changed: false, notes: [{ code: 'NOTE', detail: 'Nothing needed changing.' }] });
  assert.equal(await f.editor.confirm(f.editor.review({ clear: true })), true);
  assert.equal(f.editor.state.message, 'Already cleared. Nothing needed changing.');
});

test('slow old status reads cannot overwrite a newer household selection', async () => {
  const f = fixture(); await f.editor.load(); const slow = deferred(); f.replies.djSteeringStatus.push(slow.promise);
  const first = f.editor.select('Kitchen@S1');
  f.replies.djSteeringStatus.push({ ...status(), running: false }); await f.editor.select('Kitchen@S2');
  slow.resolve(status()); assert.equal(await first, false);
  assert.equal(f.editor.state.target, 'Kitchen@S2'); assert.equal(f.editor.state.status.running, false);
});

test('refresh preserves a stable selected group and never substitutes a changed one', async () => {
  const f = fixture(); await f.open(); await f.editor.load(); assert.equal(f.editor.state.target, 'Kitchen@S1');
  const newer = groups(); newer[0].members.push('Hall'); f.replies.djSteeringZones.push(newer);
  await f.editor.load(); assert.equal(f.editor.state.target, ''); assert.equal(f.editor.state.ready, false);
  assert.equal(f.editor.state.status, null);
});

test('a read failure does not invent rooms or forget to disable stale observations', async () => {
  const f = fixture(); await f.open();
  f.replies.djSteeringMoods.push(Object.assign(new Error('No DJ library.'), { hint: 'Connect Spotify.' }));
  assert.equal(await f.editor.load(), false);
  assert.equal(f.editor.state.ready, false); assert.equal(f.editor.state.target, '');
  assert.match(f.editor.state.message, /Connect Spotify/);
});

test('disposal stops publication after a sent save and prevents any later work', async () => {
  let publications = 0; const wait = deferred(); const calls = [];
  const editor = dj.model({ changed: () => publications++, call: async (name) => {
    calls.push(name); return { djSteeringZones: groups, djSteeringMoods: moods, djSteeringStatus: status,
      djSteeringWrite: () => wait.promise }[name]();
  } });
  await editor.load(); await editor.select('Kitchen@S1');
  const saving = editor.confirm(editor.review(draft())); await tick(); editor.dispose();
  const count = publications, requests = calls.length; wait.resolve(saved()); assert.equal(await saving, true);
  assert.equal(publications, count); assert.equal(calls.length, requests); assert.equal(await editor.load(), false);
});

test('assets are shipped and tested without adding permissive browser policy', () => {
  const root = path.resolve(__dirname, '..');
  const registry = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  assert.match(registry, /include_str!\("\.\.\/remote\/dj-steering\.js"\)/);
  assert.match(registry, /\{STEERING_SCRIPT\}/); assert.match(registry, /\{STEERING_STYLE\}/);
  assert.match(registry, /script-src 'self'/); assert.match(registry, /frame-ancestors 'none'/);
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.match(workflow, /run: node --test scripts\/test-remote-dj-preferences.cjs scripts\/test-remote-dj-steering.cjs/);
  for (const name of ['dj-steering.js', 'dj-steering.css']) {
    const asset = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', name), 'utf8');
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.ok(!asset.toLowerCase().includes(secret), `${name} contains ${secret}`);
    }
    assert.ok(!asset.includes('innerHTML'));
  }
});

test('real loopback dispatch preserves encoded group identity, exact steering and fresh readback', async (t) => {
  const received = [], name = "Living / Dining's room";
  const topology = [{ household: 'S2', coordinator_room: name, members: [name] }];
  const observation = { zone: name, running: false, steering: { source: 'none' } };
  const server = http.createServer(async (request, response) => {
    let body = ''; for await (const part of request) body += part;
    received.push({ method: request.method, path: request.url, contentType: request.headers['content-type'], body });
    response.setHeader('Content-Type', 'application/json');
    if (request.url === '/zones') response.end(JSON.stringify(topology));
    else if (request.url === '/dj/moods') response.end(JSON.stringify(moods()));
    else if (request.method === 'POST' && request.url === '/dj/steer') {
      const change = JSON.parse(body);
      observation.steering = { source: 'session', mood: change.mood, constraints: change.constraints, added: change.constraints };
      response.end(JSON.stringify(saved()));
    } else if (request.url === `/zones/${encodeURIComponent(name + '@S2')}/dj`) response.end(JSON.stringify(observation));
    else { response.statusCode = 404; response.end('{}'); }
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); return new Promise((resolve) => server.close(resolve)); });
  const editor = dj.model({ call: async (name, { params = {}, body } = {}) => {
    const [method, template] = dj.routes[name];
    const route = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    const response = await fetch(`http://127.0.0.1:${server.address().port}${route}`, {
      method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined,
    });
    assert.equal(response.status, 200); return response.json();
  } });
  await editor.load(); await editor.select(name + '@S2');
  assert.equal(await editor.confirm(editor.review(draft())), true);
  const posts = received.filter((r) => r.method === 'POST'); assert.equal(posts.length, 1);
  assert.equal(posts[0].path, '/dj/steer'); assert.equal(posts[0].contentType, 'application/json');
  assert.deepEqual(JSON.parse(posts[0].body), { zone: name + '@S2', mood: 'calm',
    constraints: { exclude_artists: ['Earth, Wind & Fire'], energy_bias: -1 }, for_secs: 5400 });
  assert.equal(editor.state.status.steering.mood, 'calm'); assert.equal(editor.state.status.running, false);
});
