'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const Schedules = require('../crates/fsonos-api/src/remote/schedules.js');

const row = (extra = {}) => ({ id: 7, when: 'weekdays 07:30', action: 'dj_start',
  zone: 'Studio@S2', mood: 'bright', summary: 'start DJ in Studio@S2', creator: 'test-client',
  enabled: true, next: '2030-01-15T07:30:00-05:00', ...extra });
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};
function fixture(initial = [row()]) {
  let rows = structuredClone(initial);
  const writes = [];
  let reads = 0;
  const call = async (name, options) => {
    if (name === 'schedules') { reads++; return structuredClone(rows); }
    writes.push({ name, ...options });
    const found = rows.find((r) => r.id === options.body.id);
    assert.ok(found);
    if (name === 'scheduleRemove') rows = rows.filter((r) => r !== found);
    else found.enabled = name === 'scheduleResume';
    return structuredClone(found);
  };
  return { control: Schedules.model({ call }), call, writes,
    set: (value) => { rows = structuredClone(value); }, reads: () => reads };
}

async function reviewed(f, action = 'pause', id = 7) {
  assert.equal(await f.control.refresh(), true);
  assert.equal(f.control.review(action, id), true);
  return f.control.state.pending;
}

test('all four schedule actions decode without borrowing unrelated fields', () => {
  for (const value of [row(), row({ action: 'pause', mood: null, fade_secs: 600 }),
    row({ action: 'volume', mood: null, volume: 0 }),
    row({ action: 'apply_scene', zone: null, mood: null, scene: 'Evening' })]) {
    const decoded = Schedules.scheduleOf(value);
    assert.equal(decoded.id, 7);
    assert.equal(Object.isFrozen(decoded), true);
  }
});

test('malformed, unsafe, ambiguous and contradictory server data is refused', () => {
  for (const change of [{ id: 0 }, { id: 2 ** 53 }, { id: '7' }, { enabled: 1 },
    { when: '' }, { creator: '' }, { next: '2030-01-15T07:30:00' }, { last_fired: 'never' },
    { action: '__proto__' }, { action: 'delete' }, { zone: '' }, { volume: 42 },
    { action: 'volume', mood: null, volume: 101 }, { action: 'pause', mood: null, fade_secs: -1 }]) {
    assert.throws(() => Schedules.scheduleOf(row(change)));
  }
  assert.throws(() => Schedules.schedulesOf({}));
  assert.throws(() => Schedules.schedulesOf([row(), row()]));
});

test('reviewing, cancelling and opening the list never writes', async () => {
  const f = fixture();
  await reviewed(f);
  f.control.cancel();
  assert.equal(await f.control.confirm(null), false);
  assert.equal(f.control.review('resume', 7), false);
  assert.equal(f.control.review('remove', 8), false);
  assert.deepEqual(f.writes, []);
});

test('pause re-reads the definition and sends the exact ID once', async () => {
  const f = fixture();
  const pending = await reviewed(f);
  const first = f.control.confirm(pending);
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(await first, true);
  assert.deepEqual(f.writes, [{ name: 'schedulePause', body: { id: 7 } }]);
  assert.equal(f.reads(), 2);
  assert.equal(f.control.state.loaded, false);
  assert.match(f.control.state.message, /paused/);
  assert.equal(f.control.review('remove', 7), false);
});

test('resume and removal use their native endpoints without replaying music', async () => {
  for (const [action, enabled, route] of [['resume', false, 'scheduleResume'], ['remove', true, 'scheduleRemove']]) {
    const f = fixture([row({ enabled, next: enabled ? row().next : null })]);
    assert.equal(await f.control.confirm(await reviewed(f, action)), true);
    assert.deepEqual(f.writes, [{ name: route, body: { id: 7 } }]);
  }
});

test('a stale confirmation cannot confirm a newer review', async () => {
  const f = fixture([row(), row({ id: 8 })]);
  const old = await reviewed(f);
  f.control.review('remove', 8);
  const latest = f.control.state.pending;
  assert.equal(await f.control.confirm(old), false);
  assert.equal(f.control.state.pending, latest);
  assert.deepEqual(f.writes, []);
});

test('changed definitions, creators, enabled flags and removed rows abort before writes', async () => {
  for (const changed of [[], [row({ zone: 'Elsewhere@S1' })], [row({ creator: 'another-client' })],
    [row({ enabled: false })], [row({ when: 'daily 03:00' })], [row({ mood: 'focus' })]]) {
    const f = fixture();
    const pending = await reviewed(f);
    f.set(changed);
    assert.equal(await f.control.confirm(pending), false);
    assert.deepEqual(f.writes, []);
    assert.match(f.control.state.message, /changed or was removed/);
    assert.equal(f.control.state.loaded, false);
  }
});

test('a normal scheduler tick does not invalidate a reviewed definition', async () => {
  const f = fixture();
  const pending = await reviewed(f);
  f.set([row({ last_fired: row().next, next: '2030-01-16T07:30:00-05:00' })]);
  assert.equal(await f.control.confirm(pending), true);
});

test('refresh and close invalidate unsent reviews', async () => {
  const f = fixture();
  const pending = await reviewed(f);
  await f.control.refresh();
  assert.equal(await f.control.confirm(pending), false);
  f.control.review('pause', 7);
  const second = f.control.state.pending;
  f.control.close();
  assert.equal(await f.control.confirm(second), false);
  assert.deepEqual(f.writes, []);
});

test('closing or disposing during preflight never sends the write', async () => {
  for (const end of ['close', 'dispose']) {
    const wait = deferred();
    let reads = 0;
    let writes = 0;
    const control = Schedules.model({ call: async (name) => {
      if (name !== 'schedules') { writes++; return row({ enabled: false }); }
      return ++reads === 1 ? [row()] : wait.promise;
    } });
    await control.refresh();
    control.review('pause', 7);
    const work = control.confirm(control.state.pending);
    control[end]();
    wait.resolve([row()]);
    assert.equal(await work, false);
    assert.equal(writes, 0);
    assert.equal(control.state.writing, false);
  }
});

test('superseded or closed list reads cannot replace current state', async () => {
  const old = deferred();
  let reads = 0;
  const control = Schedules.model({ call: async () => ++reads === 1 ? old.promise : [row({ id: 8 })] });
  const first = control.refresh();
  await control.refresh();
  old.resolve([row()]);
  assert.equal(await first, false);
  assert.equal(control.state.rows[0].id, 8);
  const blocked = deferred();
  const other = Schedules.model({ call: () => blocked.promise });
  const read = other.refresh();
  other.close();
  blocked.resolve([row()]);
  assert.equal(await read, false);
  assert.equal(other.state.loaded, false);
});

test('failed reads cannot leave a stale list writable', async () => {
  let fail = false;
  const control = Schedules.model({ call: async () => { if (fail) throw new Error('Offline'); return [row()]; } });
  await control.refresh();
  fail = true;
  assert.equal(await control.refresh(), false);
  assert.deepEqual(control.state.rows, []);
  assert.equal(control.review('pause', 7), false);
  assert.match(control.state.message, /Offline/);
});

test('uncertain writes and contradictory success replies require a fresh read, never a retry', async () => {
  for (const reply of [() => { throw new Error('Connection closed'); }, () => row(),
    () => row({ id: 8, enabled: false }), () => null]) {
    let writes = 0;
    const control = Schedules.model({ call: async (name) => {
      if (name === 'schedules') return [row()];
      writes++;
      return reply();
    } });
    await control.refresh();
    control.review('pause', 7);
    const pending = control.state.pending;
    assert.equal(await control.confirm(pending), false);
    assert.match(control.state.message, /outcome may be uncertain/);
    assert.equal(await control.confirm(pending), false);
    assert.equal(control.review('pause', 7), false);
    assert.equal(writes, 1);
  }
});

test('an observer failure does not turn an acknowledged write into a failure', async () => {
  const f = fixture();
  const control = Schedules.model({ call: f.call, changed: () => { throw new Error('View failure'); } });
  await control.refresh();
  control.review('pause', 7);
  assert.equal(await control.confirm(control.state.pending), true);
  assert.equal(f.writes.length, 1);
});

test('a sent request remains single-flight when the panel closes', async () => {
  const write = deferred();
  let writes = 0;
  const control = Schedules.model({ call: async (name) => {
    if (name === 'schedules') return [row()];
    writes++;
    return write.promise;
  } });
  await control.refresh();
  control.review('pause', 7);
  const work = control.confirm(control.state.pending);
  await new Promise(setImmediate);
  control.close();
  assert.equal(await control.refresh(), false);
  assert.equal(control.state.writing, true);
  write.resolve(row({ enabled: false }));
  assert.equal(await work, true);
  assert.equal(writes, 1);
  assert.equal(control.state.open, false);
  assert.equal(control.state.message, '');
});

test('native route map dispatches actual same-origin JSON over a localhost socket', async (t) => {
  const requests = [];
  const server = http.createServer(async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    requests.push({ method: req.method, path: req.url, body, type: req.headers['content-type'] });
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.method === 'GET' ? [row()] : row({ enabled: false })));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`;
  const control = Schedules.model({ call: async (name, { body } = {}) => {
    const [method, path] = Schedules.routes[name];
    const res = await fetch(base + path, { method, ...(body ? {
      headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) } : {}) });
    assert.equal(res.ok, true);
    return res.json();
  } });
  await control.refresh();
  control.review('pause', 7);
  assert.equal(await control.confirm(control.state.pending), true);
  assert.deepEqual(requests.map((r) => [r.method, r.path]), [
    ['GET', '/schedules'], ['GET', '/schedules'], ['POST', '/schedules/pause']]);
  assert.equal(requests[2].body, '{"id":7}');
  assert.equal(requests[2].type, 'application/json');
});

const groups = () => [{ household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Den'] },
  { household: 'S1', coordinator_room: 'Studio', members: ['Studio'] }];
const scenes = () => [{ name: 'Evening & quiet', saved: 1700000000, groups: [{ coordinator: 'Studio',
  members: ['Den'], source: { kind: 'keep' } }], volumes: { Studio: 20 }, mutes: { Den: false } }];
const moods = () => ({ moods: [{ name: 'bright', constraints: { energy_bias: 10 } }] });
function builderFixture() {
  const data = { rows: [], groups: groups(), scenes: scenes(), moods: moods() };
  const calls = [];
  const writes = [];
  let fail = null;
  let reply = null;
  const call = async (name, options) => {
    calls.push({ name, ...options });
    if (fail === name) throw new Error(`${name} unavailable`);
    const value = { schedules: data.rows, scheduleZones: data.groups,
      scheduleScenes: data.scenes, scheduleMoods: data.moods };
    if (Object.hasOwn(value, name)) return structuredClone(value[name]);
    assert.equal(name, 'scheduleAdd');
    writes.push(structuredClone(options.body));
    if (reply) return reply(options.body);
    const body = options.body;
    const when = body.when.startsWith('in ') ? 'once 2030-01-15T09:15:00-05:00'
      : body.when.includes('T') ? `once ${body.when}` : body.when;
    const result = { id: 99, ...body, when, summary: 'Saved routine', creator: 'test-client',
      enabled: true, next: when.startsWith('once ') ? when.slice(5) : '2030-01-15T07:30:00-05:00' };
    data.rows.push(result);
    return result;
  };
  const control = Schedules.model({ call });
  return { control, data, writes, calls, call, fail: (value) => { fail = value; }, reply: (value) => { reply = value; } };
}
async function draft(f, action = 'dj_start') {
  assert.equal(await f.control.refresh(), true);
  assert.equal(await f.control.beginCreate(action), true);
  f.control.edit(action === 'apply_scene' ? 'scene' : 'zone', action === 'apply_scene' ? 'Evening & quiet' : 'Studio@S2');
}

test('all 127 nonempty weekday subsets produce canonical house-time specs', () => {
  const days = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
  for (let mask = 1; mask < 128; mask++) {
    const chosen = days.filter((_, i) => mask & (1 << i));
    const expected = { 127: 'daily', 31: 'weekdays', 96: 'weekends' }[mask] || chosen.join(',');
    const timing = Schedules.timeOf({ cadence: 'custom', time: '23:59', days: [...chosen].reverse() });
    assert.deepEqual(timing, { kind: 'weekly', when: `${expected} 23:59` });
  }
});

test('timing rejects implicit zones, normalized dates, invalid wall times and huge delays', () => {
  for (const value of [
    { cadence: 'custom', days: [], time: '07:30' },
    { cadence: 'daily', time: '24:00' }, { cadence: 'daily', time: '7:00' },
    { cadence: 'once', at: '2030-01-15T07:30:00' }, { cadence: 'once', at: '2030-02-30T07:30:00Z' },
    { cadence: 'once', at: '2100-02-29T07:30:00Z' }, { cadence: 'once', at: '2030-01-15T24:00:00Z' },
    { cadence: 'once', at: '2030-01-15T07:30:00+25:00' },
    { cadence: 'delay', delay: '0s' }, { cadence: 'delay', delay: '366d' },
    { cadence: 'delay', delay: '1.5h' }, { cadence: 'delay', delay: '9999999999999999999999h' },
  ]) assert.throws(() => Schedules.timeOf(value), JSON.stringify(value));
  assert.equal(Schedules.timeOf({ cadence: 'delay', delay: '1H 30m' }).when, 'in 1h30m');
  assert.equal(Schedules.timeOf({ cadence: 'delay', delay: '365d' }).when, 'in 365d');
  assert.equal(Schedules.timeOf({ cadence: 'once', at: '2000-02-29T07:30:00+05:30' }).when, '2000-02-29T07:30:00+05:30');
});

test('new drafts do not choose a destination or write without review', async () => {
  const f = builderFixture();
  await f.control.refresh();
  await f.control.beginCreate();
  assert.equal(f.control.state.draft.zone, '');
  assert.equal(f.control.reviewCreate(), false);
  assert.equal(await f.control.confirm(null), false);
  assert.deepEqual(f.writes, []);
  assert.deepEqual(f.control.state.options.rooms.map((r) => r.target), ['Studio@S2', 'Studio@S1']);
});

test('four actions emit only their fields through the native creation route', async () => {
  const examples = [
    ['dj_start', { mood: 'bright' }, { when: 'weekdays 07:30', action: 'dj_start', zone: 'Studio@S2', mood: 'bright' }],
    ['pause', { fade_secs: '600' }, { when: 'weekdays 07:30', action: 'pause', zone: 'Studio@S2', fade_secs: 600 }],
    ['volume', { zone: 'Den@S2', volume: '0' }, { when: 'weekdays 07:30', action: 'volume', zone: 'Den@S2', volume: 0 }],
    ['apply_scene', {}, { when: 'weekdays 07:30', action: 'apply_scene', scene: 'Evening & quiet' }],
  ];
  for (const [action, edits, expected] of examples) {
    const f = builderFixture();
    await draft(f, action);
    for (const [key, value] of Object.entries(edits)) assert.equal(f.control.edit(key, value), true);
    assert.equal(f.control.reviewCreate(), true);
    const p = f.control.state.pending;
    assert.equal(Object.isFrozen(p.body), true);
    const first = f.control.confirm(p);
    assert.equal(await f.control.confirm(p), false);
    assert.equal(await first, true);
    assert.deepEqual(f.writes, [expected]);
    assert.match(f.control.state.message, /Created schedule #99/);
    assert.equal(f.control.state.draft, null);
    assert.equal(f.control.state.loaded, false);
  }
});

test('volume targets physical member rooms while playback targets only group leaders', async () => {
  const f = builderFixture();
  await draft(f, 'volume');
  assert.deepEqual(f.control.state.options.rooms.map((r) => r.target), ['Studio@S2', 'Den@S2', 'Studio@S1']);
  await f.control.beginCreate('pause');
  f.control.edit('zone', 'Den@S2');
  assert.equal(f.control.reviewCreate(), false);
});

test('out-of-range, fractional, blank volume/fade and missing mood do not review', async () => {
  for (const [action, field, values] of [['volume', 'volume', ['101', '-1', '1.5', '', '1e2']],
    ['pause', 'fade_secs', ['601', '-1', '0.5']], ['dj_start', 'mood', ['nonexistent']]]) {
    const f = builderFixture();
    await draft(f, action);
    for (const value of values) { f.control.edit(field, value); assert.equal(f.control.reviewCreate(), false); }
    assert.deepEqual(f.writes, []);
  }
});

test('absolute instants match equivalent returned offsets; delays use the daemon deadline', async () => {
  for (const [cadence, field, value] of [['once', 'at', '2030-01-15T09:15:00-05:00'], ['delay', 'delay', '45m']]) {
    const f = builderFixture();
    await draft(f, 'pause');
    f.control.edit('cadence', cadence);
    f.control.edit(field, value);
    f.control.reviewCreate();
    f.reply((body) => ({ id: 99, ...body, when: 'once 2030-01-15T14:15:00+00:00', summary: 'Saved routine',
      creator: 'test-client', enabled: true, next: '2030-01-15T14:15:00+00:00' }));
    assert.equal(await f.control.confirm(f.control.state.pending), true);
    assert.match(f.control.state.message, /2030-01-15T14:15:00/);
  }
});

test('edits consume old reviews, and switching actions clears stale action-specific fields', async () => {
  const f = builderFixture();
  await draft(f);
  f.control.edit('mood', 'bright');
  f.control.reviewCreate();
  const old = f.control.state.pending;
  f.control.edit('time', '08:00');
  assert.equal(await f.control.confirm(old), false);
  await f.control.beginCreate('volume');
  assert.equal(f.control.state.draft.mood, '');
  assert.equal(f.control.state.draft.zone, '');
  assert.equal(await f.control.confirm(old), false);
  assert.deepEqual(f.writes, []);
});

test('changed topology, scene content or mood constraints block creation before writes', async () => {
  for (const action of ['volume', 'pause', 'dj_start', 'apply_scene']) {
    const f = builderFixture();
    await draft(f, action);
    if (action === 'dj_start') f.control.edit('mood', 'bright');
    f.control.reviewCreate();
    if (action === 'apply_scene') f.data.scenes[0].volumes.Studio = 30;
    else if (action === 'dj_start') f.data.moods.moods[0].constraints.energy_bias = 90;
    else f.data.groups[0].members.pop();
    assert.equal(await f.control.confirm(f.control.state.pending), false);
    assert.deepEqual(f.writes, []);
    assert.match(f.control.state.message, /changed/);
  }
});

test('duplicate explicit routines, including paused ones, are detected again before dispatch', async () => {
  for (const enabled of [true, false]) {
    const f = builderFixture();
    await draft(f);
    f.control.reviewCreate();
    f.data.rows = [row({ mood: null, enabled })];
    assert.equal(await f.control.confirm(f.control.state.pending), false);
    assert.deepEqual(f.writes, []);
    assert.match(f.control.state.message, /equivalent schedule #7/);
    await f.control.refresh();
    await f.control.beginCreate();
    f.control.edit('zone', 'Studio@S2');
    assert.equal(f.control.reviewCreate(), false);
  }
});

test('failed creation choices do not disable management or unrelated routine types', async () => {
  const f = builderFixture();
  await f.control.refresh();
  f.fail('scheduleMoods');
  assert.equal(await f.control.beginCreate(), false);
  assert.equal(f.control.state.loading, false);
  assert.equal(f.control.state.loaded, true);
  assert.equal(await f.control.beginCreate('pause'), true);
  f.control.edit('zone', 'Studio@S2');
  assert.equal(f.control.reviewCreate(), true);
  assert.equal(await f.control.confirm(f.control.state.pending), true);
});

test('malformed or ambiguous choices cannot become destinations', () => {
  assert.throws(() => Schedules.roomsOf([...groups(), ...groups()]));
  assert.throws(() => Schedules.scenesOf([...scenes(), ...scenes()]));
  assert.throws(() => Schedules.scenesOf([{ ...scenes()[0], volumes: { Studio: 101 } }]));
  assert.throws(() => Schedules.moodsOf({ moods: [moods().moods[0], { name: 'BRIGHT', constraints: {} }] }));
  assert.throws(() => Schedules.roomsOf([{ ...groups()[0], members: ['Den'] }]));
});

test('closing creation during option loading does not resurrect its draft', async () => {
  const wait = deferred();
  const f = builderFixture();
  const control = Schedules.model({ call: (name, args) => name === 'scheduleZones' ? wait.promise : f.call(name, args) });
  await control.refresh();
  const work = control.beginCreate('pause');
  control.close();
  wait.resolve(groups());
  assert.equal(await work, false);
  assert.equal(control.state.open, false);
  assert.equal(control.state.draft, null);
  assert.equal(control.state.options, null);
});

test('a close during creation preflight cancels only an unsent request', async () => {
  const wait = deferred();
  const f = builderFixture();
  let zones = 0;
  const control = Schedules.model({ call: (name, args) => name === 'scheduleZones' && ++zones === 2 ? wait.promise : f.call(name, args) });
  await control.refresh(); await control.beginCreate('pause');
  control.edit('zone', 'Studio@S2'); control.reviewCreate();
  const work = control.confirm(control.state.pending);
  await new Promise(setImmediate);
  control.close(); wait.resolve(groups());
  assert.equal(await work, false);
  assert.deepEqual(f.writes, []);
});

test('a lost create reply cannot be retried from the consumed form', async () => {
  const f = builderFixture();
  await draft(f, 'volume'); f.control.reviewCreate();
  const pending = f.control.state.pending;
  f.reply(() => { throw new Error('Reply lost after saving'); });
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(await f.control.beginCreate('volume'), false);
  assert.equal(f.control.state.draft, null);
  assert.equal(f.writes.length, 1);
  assert.match(f.control.state.message, /uncertain/);
});

test('contradictory add replies never claim a verified routine', async () => {
  for (const extra of [{ zone: 'Elsewhere@S1' }, { action: 'dj_start', fade_secs: null },
    { enabled: false }, { when: 'daily 09:00' }, { next: null }]) {
    const f = builderFixture();
    await draft(f, 'pause'); f.control.reviewCreate();
    f.reply((body) => ({ id: 99, ...body, summary: 'Saved routine', creator: 'test-client',
      enabled: true, next: '2030-01-15T07:30:00Z', ...extra }));
    assert.equal(await f.control.confirm(f.control.state.pending), false);
    assert.match(f.control.state.message, /uncertain/);
    assert.equal(f.writes.length, 1);
  }
});

test('creation and management share the same in-flight write exclusion', async () => {
  const wait = deferred(); const f = builderFixture();
  f.data.rows = [row()]; await draft(f, 'volume'); f.control.reviewCreate();
  f.reply((body) => wait.promise.then(() => ({ id: 99, ...body, summary: 'Saved routine', creator: 'test-client',
    enabled: true, next: '2030-01-15T07:30:00Z' })));
  const work = f.control.confirm(f.control.state.pending);
  await new Promise(setImmediate);
  assert.equal(f.control.review('remove', 7), false);
  assert.equal(await f.control.refresh(), false);
  assert.equal(f.control.edit('volume', '100'), false);
  f.control.close();
  assert.equal(await f.control.beginCreate(), false);
  wait.resolve(); assert.equal(await work, true);
  assert.equal(f.writes.length, 1);
});

test('real localhost creation dispatch preserves the exact reviewed fields for every action', async (t) => {
  const requests = [];
  let id = 200;
  const server = http.createServer(async (req, res) => {
    let text = ''; for await (const chunk of req) text += chunk;
    requests.push([req.method, req.url, text, req.headers['content-type']]);
    const data = { '/schedules': [], '/zones': groups(), '/scenes': scenes(), '/dj/moods': moods() };
    const body = text ? JSON.parse(text) : null;
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(body ? { id: id++, ...body, summary: 'Saved routine', creator: 'test-client',
      enabled: true, next: '2030-01-15T07:30:00-05:00' } : data[req.url]));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  t.after(() => new Promise((r) => server.close(r)));
  for (const action of ['dj_start', 'pause', 'volume', 'apply_scene']) {
    const control = Schedules.model({ call: async (name, { body } = {}) => {
      const [method, path] = Schedules.routes[name];
      const response = await fetch(`http://127.0.0.1:${server.address().port}${path}`, { method,
        ...(body ? { body: JSON.stringify(body), headers: { 'Content-Type': 'application/json' } } : {}) });
      return response.json();
    } });
    await control.refresh(); await control.beginCreate(action);
    control.edit(action === 'apply_scene' ? 'scene' : 'zone', action === 'apply_scene' ? 'Evening & quiet' : 'Studio@S2');
    control.reviewCreate(); const pending = control.state.pending;
    assert.equal(await control.confirm(pending), true);
    assert.deepEqual(requests.at(-1), ['POST', '/schedules', JSON.stringify(pending.body), 'application/json']);
  }
  assert.equal(requests.filter((r) => r[0] === 'POST').length, 4);
});
