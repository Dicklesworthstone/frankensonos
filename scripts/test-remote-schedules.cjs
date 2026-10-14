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
