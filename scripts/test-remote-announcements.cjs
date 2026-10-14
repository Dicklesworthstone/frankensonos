// Executes the shipped controller; no npm dependencies or speaker access.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const Remote = require('../crates/fsonos-api/src/remote/announcements.js');
const clone = (v) => JSON.parse(JSON.stringify(v));
const groups = () => [
  { household: 'S1', coordinator_room: 'Room', members: ['Room', 'Side'] },
  { household: 'S2', coordinator_room: 'Room', members: ['Room'] },
];
const good = () => ({ done: 'announced; everything put back', clean: true,
  households: [{ lead: 'Room', rooms: ['Room'], levels: [{ room: 'Room', volume: 20 }], outcome: 'finished' }] });
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture({ answer = good(), read = groups, completed } = {}) {
  const calls = [];
  const c = Remote.model({ completed, call: async (route, args) => {
    calls.push([route, args]);
    if (route === 'zones') return typeof read === 'function' ? read() : read;
    if (answer instanceof Error) throw answer;
    return answer;
  } });
  return { c, calls };
}
async function prepare(c, target = 'Room@S2') {
  await c.refresh();
  c.select(target, true);
  c.field('words', ' Dinner is ready ');
  assert.equal(c.review(), true);
}

test('initialization is lazy and no omitted-room broadcast can be submitted', async () => {
  const { c, calls } = fixture();
  assert.equal(calls.length, 0);
  assert.equal(c.review(), false);
  assert.equal(await c.confirm(), false);
  await c.refresh(); c.field('words', 'Hello');
  assert.equal(c.review(), false);
  assert.match(c.state.message, /Select at least one/);
  assert.deepEqual(calls.map(([route]) => route), ['zones']);
});

test('speech sends the exact reviewed text, voice, level and qualified rooms once', async () => {
  const { c, calls } = fixture();
  await prepare(c);
  c.field('voice', ' en '); c.field('volume', '0'); c.select('Side@S1', true);
  assert.equal(c.review(), true);
  const review = c.state.pending;
  assert.equal(Object.isFrozen(review.body.rooms), true);
  assert.equal(await c.confirm(review), true);
  assert.deepEqual(calls[2], ['announceAudio', { body: {
    rooms: ['Side@S1', 'Room@S2'], volume: 0, text: 'Dinner is ready', voice: 'en',
  } }]);
  assert.equal(calls.length, 3);
  assert.equal(c.state.report.clean, true);
  assert.equal(c.state.report.households[0].levels[0].volume, 20);
  assert.equal(await c.confirm(review), false);
  assert.equal(c.review(), false, 'a new announcement requires a fresh room read');
});

test('each generated chime excludes stale speech and voice fields', async () => {
  for (const chime of ['bell', 'beep', 'rise']) {
    const { c, calls } = fixture();
    await prepare(c); c.field('voice', 'voice'); c.field('source', 'chime'); c.field('chime', chime);
    assert.equal(c.review(), true); assert.equal(await c.confirm(), true);
    assert.deepEqual(calls[2][1].body, { rooms: ['Room@S2'], volume: 35, chime });
  }
});

test('same-name household rooms are distinct and neither is preselected', async () => {
  const { c } = fixture(); await c.refresh();
  assert.equal(c.state.selected.size, 0);
  assert.equal(c.select('Room', true), false);
  c.select('Room@S1', true); c.field('words', 'One'); c.review();
  assert.deepEqual(c.state.pending.body.rooms, ['Room@S1']);
});

test('speech length counts Unicode characters and invalid input never reaches the daemon', async () => {
  const { c, calls } = fixture(); await prepare(c);
  for (const words of ['', '  ', 'a'.repeat(1001), '\ud800']) {
    c.field('words', words); assert.equal(c.review(), false);
  }
  c.field('words', '🎵'.repeat(1000)); assert.equal(c.review(), true);
  assert.equal(c.state.pending.body.text.length, 2000);
  assert.equal(calls.length, 1);
});

test('invalid volume, voice and chime choices never become defaults', async () => {
  for (const [field, invalid] of [
    ['volume', ['', '-1', '101', '1.5', 'NaN', '1e2', ' 35']],
    ['voice', ['a'.repeat(257), 'vo\nice', '\ud800']],
    ['chime', ['', 'ding', '__proto__']],
  ]) {
    for (const value of invalid) {
      const { c, calls } = fixture(); await prepare(c);
      if (field === 'chime') c.field('source', 'chime');
      c.field(field, value); assert.equal(c.review(), false, `${field}: ${JSON.stringify(value)}`);
      assert.equal(calls.length, 1);
    }
  }
});

test('room reads reject partial and conflicting topology, but empty discovery is valid', async () => {
  for (const read of [null, {}, [{ household: 'S2', coordinator_room: 'Room' }],
    [{ ...groups()[0], members: ['Room', 'Room'] }], [groups()[0], groups()[0]],
    [{ ...groups()[0], members: ['Side'] }]]) {
    const { c } = fixture({ read }); assert.equal(await c.refresh(), false); assert.equal(c.review(), false);
  }
  const { c } = fixture({ read: [] }); assert.equal(await c.refresh(), true); assert.equal(c.review(), false);
});

test('edited, cancelled, refreshed and superseded reviews cannot be confirmed', async () => {
  for (const edit of [
    (c) => c.field('words', 'Different'), (c) => c.field('volume', '25'),
    (c) => c.field('source', 'chime'), (c) => c.select('Side@S1', true),
    (c) => c.cancel(), (c) => c.review(), (c) => c.refresh(),
  ]) {
    const { c, calls } = fixture(); await prepare(c); const old = c.state.pending;
    await edit(c); assert.equal(await c.confirm(old), false);
    assert.ok(calls.every(([route]) => route === 'zones'));
  }
});

test('selected group membership changes prevent dispatch after review', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups()
    : [{ ...groups()[1], members: ['Room', 'New member'] }] });
  await prepare(c); assert.equal(await c.confirm(), false);
  assert.match(c.state.message, /group changed/);
  assert.deepEqual(calls.map(([route]) => route), ['zones', 'zones']);
});

test('membership ordering and unrelated groups do not invalidate selected rooms', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups()
    : [groups()[1], { ...groups()[0], members: ['Side', 'Room'] },
      { household: 'S2', coordinator_room: 'Elsewhere', members: ['Elsewhere'] }] });
  await prepare(c, 'Side@S1'); assert.equal(await c.confirm(), true);
  assert.equal(calls.length, 3);
});

test('failed preflight reads preserve the hint and make no audio request', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => {
    if (++reads === 1) return groups();
    throw Object.assign(new Error('Unavailable'), { hint: 'Check the daemon.' });
  } });
  await prepare(c); assert.equal(await c.confirm(), false);
  assert.match(c.state.message, /Check the daemon/); assert.equal(calls.length, 2);
});

test('double confirmation, edits and refresh cannot overlap a pending preflight', async () => {
  const waiting = deferred(); let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups() : waiting.promise });
  await prepare(c); const pending = c.state.pending; const first = c.confirm(pending);
  assert.equal(await c.confirm(pending), false); assert.equal(c.field('words', 'Next'), false);
  assert.equal(await c.refresh(), false); assert.equal(c.select('Side@S1', true), false);
  waiting.resolve(groups()); assert.equal(await first, true);
  assert.equal(calls.filter(([route]) => route === 'announceAudio').length, 1);
});

test('closing, disposing, and closing then reopening cancel unsent preflights', async () => {
  for (const action of ['close', 'dispose', 'reopen']) {
    const waiting = deferred(); let reads = 0;
    const { c, calls } = fixture({ read: () => ++reads === 1 ? groups() : waiting.promise });
    await prepare(c); const work = c.confirm();
    if (action === 'dispose') c.dispose(); else c.close();
    if (action === 'reopen') await c.refresh();
    waiting.resolve(groups()); assert.equal(await work, false);
    assert.equal(calls.length, 2); assert.equal(c.state.writing, false);
  }
});

test('closing after dispatch never cancels, repeats or forgets its result', async () => {
  const waiting = deferred(); const { c, calls } = fixture({ answer: waiting.promise });
  await prepare(c); const work = c.confirm();
  await new Promise(setImmediate); assert.equal(c.state.phase, 'announcing');
  c.close(); assert.equal(c.state.writing, true); assert.equal(await c.refresh(), false);
  waiting.resolve(good()); assert.equal(await work, true); assert.equal(c.state.report.clean, true);
  assert.equal(calls.length, 3);
});

test('lost, rejected or malformed write replies require explicit refresh and are not repeated', async () => {
  const invalid = [new Error('Disconnected'), null, {}, { ...good(), households: [] },
    { ...good(), clean: 'yes' }, { ...good(), households: [{ ...good().households[0], outcome: 'failed' }] },
    { ...good(), households: [{ ...good().households[0], levels: [{ room: 'Room', volume: 101 }] }] }];
  for (const answer of invalid) {
    const { c, calls } = fixture({ answer }); await prepare(c);
    assert.equal(await c.confirm(), false); assert.equal(c.review(), false); assert.equal(await c.confirm(), false);
    assert.equal(calls.length, 3); assert.equal(c.state.report, null); assert.match(c.state.message, /not repeated/);
  }
});

test('partial playback and restoration outcomes retain applied levels and all details', async () => {
  for (const outcome of ['deadline', 'never_started', 'failed']) {
    const answer = good(); answer.clean = false;
    Object.assign(answer.households[0], { outcome, error: 'Player unavailable', not_restored: ['Room volume: user changed it'] });
    const { c } = fixture({ answer }); await prepare(c); assert.equal(await c.confirm(), true);
    assert.match(c.state.message, /did not complete cleanly/);
    assert.deepEqual(c.state.report.households[0].not_restored, ['Room volume: user changed it']);
    assert.equal(c.state.report.households[0].levels[0].volume, 20);
  }
});

test('late room reads cannot replace a newer read or resurrect a closed panel', async () => {
  for (const close of [false, true]) {
    const waiting = deferred(); let reads = 0;
    const { c } = fixture({ read: () => ++reads === 1 ? waiting.promise : [groups()[1]] });
    const old = c.refresh(); if (close) c.close(); else await c.refresh();
    waiting.resolve(groups()); assert.equal(await old, false);
    assert.equal(c.state.rooms.length, close ? 0 : 1);
  }
});

test('refresh drops vanished rooms without selecting replacements', async () => {
  let reads = 0; const { c } = fixture({ read: () => ++reads === 1 ? groups() : [groups()[0]] });
  await prepare(c); await c.refresh(); assert.equal(c.state.selected.size, 0); assert.equal(c.review(), false);
});

test('status refresh failures cannot turn an acknowledged announcement into a retry', async () => {
  for (const completed of [() => { throw new Error('read failed'); }, () => Promise.reject(new Error('read failed'))]) {
    const { c, calls } = fixture({ completed }); await prepare(c);
    assert.equal(await c.confirm(), true); await new Promise(setImmediate);
    assert.equal(calls.length, 3); assert.equal(c.state.report.clean, true);
  }
});

test('disposed controllers reject every mutation and late completion callbacks', async () => {
  let completed = 0; const waiting = deferred(); const { c, calls } = fixture({ answer: waiting.promise, completed: () => completed++ });
  await prepare(c); const work = c.confirm(); await new Promise(setImmediate); c.dispose();
  waiting.resolve(good()); await work;
  assert.equal(completed, 0); assert.equal(await c.refresh(), false); assert.equal(c.review(), false);
  assert.equal(c.field('words', 'Again'), false); assert.equal(c.select('Room@S2', true), false); assert.equal(calls.length, 3);
});

test('unknown model fields and sources cannot alter a request', async () => {
  const { c } = fixture(); await prepare(c);
  for (const field of ['__proto__', 'constructor', 'pending', 'rooms']) assert.equal(c.field(field, 'anything'), false);
  assert.equal(c.field('source', 'arbitrary'), false); assert.equal(c.select('Room@S2', 'false'), false);
});

test('the embedded assets and CI include the controller without weakening browser policy', () => {
  const root = path.join(__dirname, '..');
  const embedding = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.match(embedding, /include_str!\("\.\.\/remote\/announcements\.js"\)/);
  assert.match(embedding, /include_str!\("\.\.\/remote\/announcements\.css"\)/);
  assert.match(embedding, /MODE_SCRIPT}\\n\{ANNOUNCEMENTS_SCRIPT}/);
  assert.match(embedding, /connect-src 'self'/); assert.match(embedding, /frame-ancestors 'none'/);
  assert.match(workflow, /node --test scripts\/test-remote-announcements.cjs/);
  assert.deepEqual(Remote.routes.announceAudio, ['POST', '/announce']);
  for (const name of ['announcements.js', 'announcements.css']) {
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', name), 'utf8').toLowerCase();
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.ok(!source.includes(secret), `${name}: ${secret}`);
    }
  }
});

test('the controller completes speech through a real loopback JSON endpoint', async (t) => {
  const seen = [];
  const server = http.createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    seen.push({ method: request.method, url: request.url, type: request.headers['content-type'], body: Buffer.concat(chunks).toString() });
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(request.url === '/zones' ? groups() : good()));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  const c = Remote.model({ call: async (name, { body } = {}) => {
    const [method, pathname] = name === 'zones' ? ['GET', '/zones'] : Remote.routes[name];
    const response = await fetch(`http://127.0.0.1:${server.address().port}${pathname}`, {
      method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined,
    });
    return response.json();
  } });
  await prepare(c); assert.equal(await c.confirm(), true);
  assert.equal(seen.length, 3); assert.equal(seen[2].url, '/announce'); assert.equal(seen[2].method, 'POST');
  assert.deepEqual(JSON.parse(seen[2].body), { rooms: ['Room@S2'], volume: 35, text: 'Dinner is ready' });
});
