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

test('all five schedule actions decode without borrowing unrelated fields', () => {
  for (const value of [row(), row({ action: 'pause', mood: null, fade_secs: 600 }),
    row({ action: 'volume', mood: null, volume: 0 }),
    row({ action: 'apply_scene', zone: null, mood: null, scene: 'Evening' }),
    row({ action: 'announce', zone: null, mood: null, announcement: { chime: 'bell', rooms: ['Studio@S2'] } })]) {
    const decoded = Schedules.scheduleOf(value);
    assert.equal(decoded.id, 7);
    assert.equal(Object.isFrozen(decoded), true);
  }
});

test('schedule reads retain and freeze reusable directions and refuse unrelated action fields', () => {
  const direction = { constraints: { include_genres: ['jazz'], include_artists: ['Earth, Wind & Fire'] }, for_secs: 17 };
  const decoded = Schedules.scheduleOf(row({ direction }));
  assert.deepEqual(decoded.direction, direction);
  assert.equal(Object.isFrozen(decoded.direction.constraints.include_artists), true);
  assert.deepEqual(Schedules.scheduleOf(row({ mood: null, direction: {} })).direction, { constraints: {} });
  assert.equal(Schedules.scheduleOf(row()).direction, undefined);
  for (const change of [{ direction: { extra: true } }, { direction: { for_secs: 0 } },
    { direction: { constraints: { energy_bias: 3 } } },
    { action: 'pause', mood: null, direction: {} },
    { action: 'volume', mood: null, volume: 20, direction: {} },
    { action: 'apply_scene', mood: null, zone: null, scene: 'Evening', direction: {} }]) {
    assert.throws(() => Schedules.scheduleOf(row(change)));
  }
});

test('changing a saved direction invalidates schedule management reviews before writing', async () => {
  const original = { constraints: { include_artists: ['Artist A'] }, for_secs: 7200 };
  for (const direction of [undefined, {}, { ...original, for_secs: 3600 },
    { ...original, constraints: { include_artists: ['Artist B'] } }]) {
    const f = fixture([row({ direction: original })]);
    const pending = await reviewed(f);
    f.set([row({ direction })]);
    assert.equal(await f.control.confirm(pending), false);
    assert.deepEqual(f.writes, []);
  }
});

test('the scene chooser validates DJ directions and also accepts captured unchanged-queue scenes', () => {
  const base = { name: 'Evening', saved: 1, volumes: {}, mutes: {},
    groups: [{ coordinator: 'Studio@S2', members: [], source: { kind: 'dj', direction: {} } }] };
  assert.match(Schedules.scenesOf([base])[0].details, /direction/);
  const invalid = structuredClone(base); invalid.groups[0].source.direction = { constraints: { energy_bias: 9 } };
  assert.throws(() => Schedules.scenesOf([invalid]));
  const queue = structuredClone(base); queue.groups[0].source = { kind: 'queue', queue: {
    coordinator: 'RINCON_SYNTHETIC', update_id: 17, track: 3, position_secs: 70 } };
  assert.equal(Schedules.scenesOf([queue])[0].name, 'Evening');
  queue.groups[0].source.queue.track = 0;
  assert.throws(() => Schedules.scenesOf([queue]));
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

const directionDraft = {
  include_composers: ' Bach ', exclude_composers: 'Wagner',
  include_artists: 'Earth, Wind & Fire\nMiles Davis', exclude_artists: 'Other Artist',
  include_genres: 'jazz', exclude_genres: 'metal',
  periods: 'Late Romantic', exclude_periods: 'modern',
  decades: '1960\n1970', exclude_decades: '1980',
  include_keywords: 'piano', exclude_keywords: 'vocal',
  min_work_minutes: '2', max_work_minutes: '60', energy_bias: '-1',
  allow_long_works: true, minutes: '90',
};
const fullDirection = () => ({ constraints: {
  include_composers: ['Bach'], exclude_composers: ['Wagner'],
  include_artists: ['Earth, Wind & Fire', 'Miles Davis'], exclude_artists: ['Other Artist'],
  include_genres: ['jazz'], exclude_genres: ['metal'],
  periods: ['late_romantic'], exclude_periods: ['modern'],
  decades: [1960, 1970], exclude_decades: [1980],
  include_keywords: ['piano'], exclude_keywords: ['vocal'],
  min_work_minutes: 2, max_work_minutes: 60, energy_bias: -1, allow_long_works: true,
}, for_secs: 5400 });
function fillDirection(control) {
  for (const [key, value] of Object.entries(directionDraft)) assert.equal(control.editDirection(key, value), true, key);
}
function changedDirections() {
  const changed = Object.keys(fullDirection().constraints).map((field) => {
    const direction = fullDirection();
    const value = direction.constraints[field];
    direction.constraints[field] = Array.isArray(value) ? field.includes('decades') ? [1990]
      : field.includes('periods') ? ['baroque'] : ['Changed entry']
      : typeof value === 'boolean' ? false : value + 1;
    return [field, direction];
  });
  return [...changed, ['minutes', { ...fullDirection(), for_secs: 5460 }], ['absent', undefined], ['automatic', {}]];
}

test('new automatic DJ routines explicitly clear earlier steering and remain distinct from legacy routines', async () => {
  const f = builderFixture();
  f.data.rows = [row({ mood: null })];
  await draft(f);
  assert.equal(Object.isFrozen(f.control.state.draft.direction), true);
  assert.equal(f.control.reviewCreate(), true);
  const pending = f.control.state.pending;
  assert.deepEqual(pending.body.direction, { constraints: {} });
  assert.match(pending.summary, /automatic program \(clears current steering\)/);
  assert.equal(await f.control.confirm(pending), true);
  assert.deepEqual(f.writes, [{ when: 'weekdays 07:30', action: 'dj_start', zone: 'Studio@S2',
    direction: { constraints: {} } }]);
  assert.equal(Schedules.scheduleOf(f.data.rows[0]).direction, undefined);
});

test('all sixteen DJ filters and the relative duration are reviewed and sent together once', async () => {
  const f = builderFixture(); await draft(f); fillDirection(f.control);
  assert.equal(f.control.reviewCreate(), true);
  const pending = f.control.state.pending;
  assert.deepEqual(pending.body.direction, fullDirection());
  assert.equal(Object.isFrozen(pending.body.direction), true);
  assert.equal(Object.isFrozen(pending.body.direction.constraints.include_artists), true);
  assert.throws(() => pending.body.direction.constraints.include_artists.push('Unreviewed'));
  assert.match(pending.summary, /Earth, Wind & Fire/);
  assert.match(pending.summary, /for 90 minutes on each start/);
  assert.match(pending.summary, /long works allowed/);
  assert.equal(await f.control.confirm(pending), true);
  assert.deepEqual(f.writes, [{ when: 'weekdays 07:30', action: 'dj_start', zone: 'Studio@S2', direction: fullDirection() }]);
});

test('every direction field consumes a previous review and cannot be edited during a sent create', async () => {
  for (const field of Object.keys(directionDraft)) {
    const f = builderFixture(); await draft(f); fillDirection(f.control);
    f.control.reviewCreate(); const old = f.control.state.pending;
    const changed = field === 'allow_long_works' ? false : field === 'energy_bias' ? '2'
      : ['min_work_minutes', 'max_work_minutes', 'minutes'].includes(field) ? '3'
        : field.includes('decades') ? '1990' : field.includes('periods') ? 'baroque' : 'Another entry';
    assert.equal(f.control.editDirection(field, changed), true, field);
    assert.equal(f.control.state.pending, null, field);
    assert.equal(await f.control.confirm(old), false, field);
    assert.deepEqual(f.writes, [], field);
  }
  const f = builderFixture(); await draft(f); fillDirection(f.control);
  f.control.reviewCreate(); const wait = deferred();
  f.reply((body) => wait.promise.then(() => ({ id: 99, ...body, summary: 'Saved direction', creator: 'test-client',
    enabled: true, next: '2030-01-15T07:30:00Z' })));
  const work = f.control.confirm(f.control.state.pending); await new Promise(setImmediate);
  assert.equal(f.control.editDirection('include_genres', 'rock'), false);
  wait.resolve(); assert.equal(await work, true);
  assert.deepEqual(f.writes[0].direction, fullDirection());
});

test('invalid direction drafts and timed automatic clearing cannot reach a write', async () => {
  for (const [field, value] of [['minutes', '0'], ['minutes', '10081'], ['minutes', '1.5'],
    ['periods', 'unknown period'], ['decades', '1965'], ['include_artists', 'x'.repeat(129)],
    ['include_genres', Array(33).fill('jazz').join('\n')], ['energy_bias', '3'],
    ['min_work_minutes', '601']]) {
    const f = builderFixture(); await draft(f); f.control.edit('mood', 'bright');
    assert.equal(f.control.editDirection(field, value), true);
    assert.equal(f.control.reviewCreate(), false, field);
    assert.equal(await f.control.confirm(f.control.state.pending), false);
    assert.deepEqual(f.writes, []);
  }
  const f = builderFixture(); await draft(f);
  f.control.editDirection('minutes', '60'); assert.equal(f.control.reviewCreate(), false);
  f.control.editDirection('minutes', ''); assert.equal(f.control.reviewCreate(), true);
  const pending = f.control.state.pending;
  assert.equal(f.control.editDirection('allow_long_works', 'true'), false);
  assert.equal(await f.control.confirm(pending), false);
  assert.equal(f.control.edit('direction', 'unreadable'), false);
  assert.equal(f.control.editDirection('unknown_field', ''), false);
  assert.deepEqual(f.writes, []);
});

test('duplicate routines compare every filter and duration while preserving legacy absence', async () => {
  const f = builderFixture(); f.data.rows = [row({ mood: null, direction: fullDirection() })];
  await draft(f); fillDirection(f.control);
  assert.equal(f.control.reviewCreate(), false);
  assert.match(f.control.state.message, /equivalent schedule #7/);
  for (const [field, direction] of changedDirections()) {
    f.data.rows = [row({ mood: null, direction })];
    await draft(f); fillDirection(f.control);
    assert.equal(f.control.reviewCreate(), true, field);
  }
  assert.deepEqual(f.writes, []);
});

test('create acknowledgements must retain every reviewed filter and relative duration', async () => {
  for (const [field, direction] of changedDirections()) {
    const f = builderFixture(); await draft(f); fillDirection(f.control); f.control.reviewCreate();
    f.reply((body) => ({ id: 99, ...body, direction, summary: 'Saved direction', creator: 'test-client',
      enabled: true, next: '2030-01-15T07:30:00Z' }));
    const pending = f.control.state.pending;
    assert.equal(await f.control.confirm(pending), false, field);
    assert.equal(await f.control.confirm(pending), false, field);
    assert.equal(f.writes.length, 1, field);
    assert.match(f.control.state.message, /uncertain/, field);
  }
});

test('valid long filter summaries remain readable after an acknowledged create', async () => {
  const f = builderFixture(); await draft(f);
  f.control.editDirection('include_artists', Array(20).fill(`Artist ${'x'.repeat(120)}`).join('\n'));
  assert.equal(f.control.reviewCreate(), true);
  f.reply((body) => ({ id: 99, ...body, summary: body.direction.constraints.include_artists.join('; '),
    creator: 'test-client', enabled: true, next: '2030-01-15T07:30:00Z' }));
  assert.equal(await f.control.confirm(f.control.state.pending), true);
  assert.ok(f.control.state.rows[0].summary.length > 2048);
});

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
    ['dj_start', { mood: 'bright' }, { when: 'weekdays 07:30', action: 'dj_start', zone: 'Studio@S2', mood: 'bright', direction: { constraints: {} } }],
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
  f.control.editDirection('include_genres', 'jazz');
  f.control.reviewCreate();
  const old = f.control.state.pending;
  f.control.edit('time', '08:00');
  assert.equal(await f.control.confirm(old), false);
  await f.control.beginCreate('volume');
  assert.equal(f.control.state.draft.mood, '');
  assert.equal(f.control.state.draft.zone, '');
  assert.equal(f.control.state.draft.direction, undefined);
  assert.equal(f.control.editDirection('include_genres', 'rock'), false);
  assert.equal(await f.control.confirm(old), false);
  await f.control.beginCreate('dj_start');
  assert.equal(f.control.state.draft.direction.include_genres, '');
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
    f.data.rows = [row({ mood: null, direction: {}, enabled })];
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
  for (const action of ['dj_start', 'pause', 'volume', 'apply_scene', 'announce']) {
    const control = Schedules.model({ call: async (name, { body } = {}) => {
      const [method, path] = Schedules.routes[name];
      const response = await fetch(`http://127.0.0.1:${server.address().port}${path}`, { method,
        ...(body ? { body: JSON.stringify(body), headers: { 'Content-Type': 'application/json' } } : {}) });
      return response.json();
    } });
    await control.refresh(); await control.beginCreate(action);
    control.edit(action === 'apply_scene' ? 'scene' : 'zone', action === 'apply_scene' ? 'Evening & quiet' : 'Studio@S2');
    if (action === 'announce') {
      control.edit('announcement_source', 'chime'); control.edit('announcement_chime', 'bell');
      control.edit('announcement_rooms', ['Studio@S2', 'Studio@S1']); control.edit('announcement_volume', '25');
    }
    control.reviewCreate(); const pending = control.state.pending;
    assert.equal(await control.confirm(pending), true);
    assert.deepEqual(requests.at(-1), ['POST', '/schedules', JSON.stringify(pending.body), 'application/json']);
  }
  assert.equal(requests.filter((r) => r[0] === 'POST').length, 5);
});

// A small DOM fixture runs the real element helper and both components without
// a browser or third-party DOM dependency. Layout is outside this fixture.
function domFixture() {
  const document = { activeElement: null };
  class Element {
    constructor(tag) {
      this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map(); this.events = new Map();
      this.disabled = false; this.open = false; this.type = tag === 'input' ? 'text' : tag;
      this.value = ''; this.checked = false; this.selectionStart = 0;
    }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    set textContent(value) { this.children = [String(value)]; }
    get textContent() { return this.children.map((child) => typeof child === 'string' ? child : child.textContent).join(''); }
    setAttribute(name, value) {
      this.attributes.set(name, String(value));
      if (['disabled', 'open', 'checked'].includes(name)) this[name] = true;
      else if (['type', 'id', 'value'].includes(name)) this[name] = String(value);
    }
    getAttribute(name) { return this.attributes.get(name) ?? null; }
    contains(other) { return this === other || this.children.some((child) => child instanceof Element && child.contains(other)); }
    descendants() { return this.children.flatMap((child) => child instanceof Element ? [child, ...child.descendants()] : []); }
    querySelectorAll(selector) { return this.descendants().filter((child) => child.attributes.has(selector.slice(1, -1))); }
    addEventListener(name, handler) { this.events.set(name, [...(this.events.get(name) || []), handler]); }
    async dispatch(name) { for (const handler of this.events.get(name) || []) await handler({ target: this }); }
    focus() { document.activeElement = this; }
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
  }
  document.createElement = (tag) => new Element(tag);
  const fs = require('node:fs');
  const path = require('node:path');
  const vm = require('node:vm');
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const helper = source.slice(source.indexOf('function el('), source.indexOf('\nlet toastTimer'));
  const el = vm.runInNewContext(`${helper}\nel`, { document });
  return { document, el };
}

test('the schedule DOM exposes full direction controls, preserves typing and sends the reviewed values', async (t) => {
  const prior = global.document;
  const dom = domFixture(); global.document = dom.document;
  t.after(() => { if (prior === undefined) delete global.document; else global.document = prior; });
  const f = builderFixture();
  const panel = Schedules.panel({ call: f.call, el: dom.el });
  t.after(() => panel.dispose());
  const control = (key) => {
    const found = panel.node.querySelectorAll('[data-schedule-focus]').find((node) => node.getAttribute('data-schedule-focus') === key);
    assert.ok(found, `missing control ${key}`); return found;
  };
  const editor = () => panel.node.descendants().find((node) => node.className === 'dj-direction-editor');
  panel.node.open = true; await panel.node.dispatch('toggle'); await new Promise(setImmediate);
  await control('new-create').dispatch('click');
  control('zone').value = 'Studio@S2'; await control('zone').dispatch('change');
  assert.equal(editor().open, false);
  editor().open = true; await editor().dispatch('toggle');
  for (const key of Object.keys(directionDraft)) assert.equal(control(`schedule-direction-${key}`).disabled, false, key);
  assert.equal(editor().descendants().filter((node) => node.tagName === 'TEXTAREA').length, 12);

  const artists = control('schedule-direction-include_artists');
  artists.value = 'Earth, Wind & Fire\nMiles Davis'; artists.selectionStart = 9; artists.focus();
  await artists.dispatch('input');
  assert.equal(editor().open, true);
  assert.equal(dom.document.activeElement, control('schedule-direction-include_artists'));
  assert.equal(dom.document.activeElement.selectionStart, 9);
  assert.equal(dom.document.activeElement.value, 'Earth, Wind & Fire\nMiles Davis');
  control('schedule-direction-minutes').value = '90'; await control('schedule-direction-minutes').dispatch('input');
  control('schedule-direction-allow_long_works').checked = true; await control('schedule-direction-allow_long_works').dispatch('change');
  control('schedule-direction-energy_bias').value = '-1'; await control('schedule-direction-energy_bias').dispatch('change');
  await control('review-create').dispatch('click');
  assert.match(panel.node.textContent, /Earth, Wind & Fire/);
  assert.match(panel.node.textContent, /for 90 minutes on each start/);
  assert.deepEqual(f.writes, []);

  const wait = deferred();
  f.reply((body) => wait.promise.then(() => ({ id: 99, ...body, summary: 'Saved direction', creator: 'test-client',
    enabled: true, next: '2030-01-15T07:30:00Z' })));
  const work = control('confirm').dispatch('click'); await new Promise(setImmediate);
  for (const key of Object.keys(directionDraft)) assert.equal(control(`schedule-direction-${key}`).disabled, true, key);
  wait.resolve(); await work;
  assert.deepEqual(f.writes, [{ when: 'weekdays 07:30', action: 'dj_start', zone: 'Studio@S2', direction: {
    constraints: { include_artists: ['Earth, Wind & Fire', 'Miles Davis'], energy_bias: -1, allow_long_works: true }, for_secs: 5400,
  } }]);
  assert.match(panel.node.textContent, /Created schedule #99/);
});


const announcementRow = (announcement, extra = {}) => row({
  action: 'announce', zone: null, mood: null, announcement, summary: 'Scheduled announcement', ...extra,
});

test('saved speech and chimes decode full normalized recipes and nullable wire fields', () => {
  const value = announcementRow({ text: ' Time to leave 🚪 ', chime: null, wav_base64: null,
    voice: ' Voice Name ', rooms: [' Den@S2 ', 'Studio@S1'], volume: 0 });
  const decoded = Schedules.scheduleOf(value);
  assert.deepEqual(decoded.announcement, { text: 'Time to leave 🚪', voice: 'Voice Name',
    rooms: ['Den@S2', 'Studio@S1'], volume: 0 });
  assert.equal(Object.isFrozen(decoded.announcement), true);
  assert.equal(Object.isFrozen(decoded.announcement.rooms), true);
  assert.deepEqual(Schedules.scheduleOf(announcementRow({ chime: ' BELL ', rooms: [] })).announcement,
    { chime: 'bell', rooms: [] });
  assert.equal(Schedules.schedulesOf([row({ id: 8 }), value]).length, 2);
  for (const announcement of [{}, { wav_base64: 'UklGRg==' }, { text: 'Hi', chime: 'bell' },
    { chime: 'bell', voice: 'Voice Name' }, { text: 'Hi', volume: 101 }, { chime: 'other' },
    { text: 'a'.repeat(1001) }, { text: 'Hi', rooms: [''] }, { text: 'Hi', voice: '-bad' }]) {
    assert.throws(() => Schedules.scheduleOf(announcementRow(announcement)));
  }
  assert.equal(Schedules.announcementOf({ text: '🎶'.repeat(1000), rooms: [] }).text.length, 2000);
  assert.throws(() => Schedules.scheduleOf(row({ announcement: { chime: 'bell' } })));
});

async function announcementDraft(f, source = 'text') {
  await draft(f, 'announce');
  assert.deepEqual(f.control.state.options.rooms.map((room) => room.target), ['Studio@S2', 'Den@S2', 'Studio@S1']);
  f.control.edit('announcement_source', source);
  f.control.edit('announcement_rooms', ['Den@S2', 'Studio@S1']);
  f.control.edit('announcement_volume', '25');
  if (source === 'text') {
    f.control.edit('announcement_text', 'Time to leave — bring your coat');
    f.control.edit('announcement_voice', 'Voice Name');
  } else f.control.edit('announcement_chime', 'bell');
}

test('speech and chime routines review and create only their complete nested recipe', async () => {
  for (const source of ['text', 'chime']) {
    const f = builderFixture();
    await announcementDraft(f, source);
    assert.equal(f.control.reviewCreate(), true);
    const pending = f.control.state.pending;
    assert.deepEqual(pending.body, { when: 'weekdays 07:30', action: 'announce', announcement: {
      ...(source === 'text' ? { text: 'Time to leave — bring your coat', voice: 'Voice Name' } : { chime: 'bell' }),
      rooms: ['Den@S2', 'Studio@S1'], volume: 25,
    } });
    assert.match(pending.summary, /Den@S2, Studio@S1/);
    assert.match(pending.summary, source === 'text' ? /Time to leave — bring your coat.*Voice Name/ : /bell chime/);
    assert.deepEqual(f.writes, []);
    assert.equal(await f.control.confirm(pending), true);
    assert.deepEqual(f.writes, [pending.body]);
    assert.equal(f.control.state.rows[0].announcement.volume, 25);
  }
});

test('announcement creation requires selected rooms and valid speech before review', async () => {
  const f = builderFixture(); await draft(f, 'announce');
  assert.equal(f.control.reviewCreate(), false);
  assert.match(f.control.state.message, /at least one room/);
  f.control.edit('announcement_rooms', ['Den@S2']);
  assert.equal(f.control.reviewCreate(), false);
  f.control.edit('announcement_text', '🎶'.repeat(1000));
  assert.equal(f.control.reviewCreate(), true);
  for (const [field, value] of [['announcement_volume', '101'], ['announcement_volume', '1.5'],
    ['announcement_rooms', ['Missing@S2']], ['announcement_voice', '-bad']]) {
    const invalid = builderFixture(); await announcementDraft(invalid);
    invalid.control.edit(field, value);
    assert.equal(invalid.control.reviewCreate(), false, field);
    assert.deepEqual(invalid.writes, []);
  }
});

test('every announcement edit consumes its old review and all selected rooms are rechecked', async () => {
  for (const [field, value] of [['announcement_text', 'Different words'], ['announcement_voice', 'Other Voice'],
    ['announcement_volume', '0'], ['announcement_rooms', ['Studio@S2']], ['announcement_source', 'chime']]) {
    const f = builderFixture(); await announcementDraft(f);
    assert.equal(f.control.reviewCreate(), true);
    const old = f.control.state.pending;
    assert.equal(f.control.edit(field, value), true);
    assert.equal(f.control.state.pending, null);
    assert.equal(await f.control.confirm(old), false);
    assert.deepEqual(f.writes, []);
  }
  const f = builderFixture(); await announcementDraft(f);
  f.control.reviewCreate(); const pending = f.control.state.pending;
  f.data.groups[1].members.push('Other Room');
  assert.equal(await f.control.confirm(pending), false);
  assert.match(f.control.state.message, /target group/);
  assert.deepEqual(f.writes, []);
});

test('announcement recipes participate in duplicate detection and management definition checks', async () => {
  const f = builderFixture(); await announcementDraft(f);
  f.control.reviewCreate(); const recipe = f.control.state.pending.body.announcement;
  f.data.rows = [announcementRow(recipe)];
  assert.equal(await f.control.confirm(f.control.state.pending), false);
  assert.match(f.control.state.message, /equivalent schedule #7/);
  assert.deepEqual(f.writes, []);
  for (const changed of [{ ...recipe, text: 'Different words' }, { ...recipe, volume: 0 },
    { ...recipe, voice: 'Other Voice' }, { ...recipe, rooms: ['Studio@S2'] },
    { chime: 'bell', rooms: recipe.rooms, volume: 25 }]) {
    const manager = fixture([announcementRow(recipe)]);
    const pending = await reviewed(manager);
    manager.set([announcementRow(changed)]);
    assert.equal(await manager.control.confirm(pending), false);
    assert.deepEqual(manager.writes, []);
  }
});

test('nullable server recipes acknowledge accurately and a changed announcement reply remains uncertain', async () => {
  for (const changed of [false, true]) {
    const f = builderFixture(); await announcementDraft(f);
    f.control.reviewCreate(); const pending = f.control.state.pending;
    f.reply((body) => ({ id: 99, ...body,
      announcement: { text: null, chime: null, wav_base64: null, voice: null, ...body.announcement,
        ...(changed ? { text: 'Different message' } : {}) },
      summary: 'Saved announcement', creator: 'test-client', enabled: true, next: '2030-01-15T07:30:00Z' }));
    assert.equal(await f.control.confirm(pending), !changed);
    assert.equal(f.writes.length, 1);
    if (changed) assert.match(f.control.state.message, /may already be saved/);
    assert.equal(await f.control.confirm(pending), false);
    assert.equal(f.writes.length, 1);
  }
});

test('the schedule DOM creates multi-room speech and displays the complete saved message', async (t) => {
  const prior = global.document;
  const dom = domFixture(); global.document = dom.document;
  t.after(() => { if (prior === undefined) delete global.document; else global.document = prior; });
  const f = builderFixture();
  const panel = Schedules.panel({ call: f.call, el: dom.el });
  t.after(() => panel.dispose());
  const control = (key) => {
    const found = panel.node.querySelectorAll('[data-schedule-focus]').find((node) => node.getAttribute('data-schedule-focus') === key);
    assert.ok(found, `missing control ${key}`); return found;
  };
  panel.node.open = true; await panel.node.dispatch('toggle'); await new Promise(setImmediate);
  await control('new-create').dispatch('click');
  control('action').value = 'announce'; await control('action').dispatch('change');
  const message = control('announcement_text');
  message.value = 'Time to leave — bring your coat'; message.selectionStart = 12; message.focus();
  await message.dispatch('input');
  assert.equal(dom.document.activeElement, control('announcement_text'));
  assert.equal(dom.document.activeElement.selectionStart, 12);
  control('announcement_voice').value = 'Voice Name'; await control('announcement_voice').dispatch('input');
  control('announcement_volume').value = '0'; await control('announcement_volume').dispatch('input');
  for (const target of ['Den@S2', 'Studio@S1']) {
    control(`announcement-room-${target}`).checked = true; await control(`announcement-room-${target}`).dispatch('change');
  }
  await control('review-create').dispatch('click');
  assert.match(panel.node.textContent, /Time to leave — bring your coat/);
  assert.match(panel.node.textContent, /Voice Name/);
  assert.match(panel.node.textContent, /volume 0/);
  assert.deepEqual(f.writes, []);
  await control('confirm').dispatch('click');
  assert.deepEqual(f.writes, [{ when: 'weekdays 07:30', action: 'announce', announcement: {
    text: 'Time to leave — bring your coat', voice: 'Voice Name', rooms: ['Den@S2', 'Studio@S1'], volume: 0,
  } }]);
  await control('refresh').dispatch('click');
  assert.match(panel.node.textContent, /Time to leave — bring your coat/);
  assert.match(panel.node.textContent, /Creator: test-client/);
  assert.equal(control('toggle-99').disabled, false);
});
