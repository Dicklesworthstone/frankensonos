'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const prefs = require('../crates/fsonos-api/src/remote/dj-preferences.js');

function defaults() {
  return { energy: null, explicit: false,
    favor: { genres: [], artists: [], eras: [], moods: [] },
    avoid: { genres: [], artists: [], eras: [], moods: [] },
    pin: { artists: [], albums: [], tracks: [] }, ban: { artists: [], albums: [], tracks: [] } };
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function saved(preferences = defaults()) { return { done: 'Preference saved.', changed: true, preferences }; }
function fixture() {
  const calls = [], reads = [], writes = [];
  const editor = prefs.model({ call: async (name, args) => {
    calls.push([name, args]);
    if (name === 'djPreferencesRead') return reads.length ? reads.shift() : defaults();
    if (name === 'djPreferencesWrite') return writes.length ? writes.shift() : saved();
    throw new Error(`Unexpected route ${name}`);
  } });
  return { editor, calls, reads, writes };
}

test('read/write routes use the existing preferences API, with no playback route', () => {
  assert.deepEqual(prefs.routes, {
    djPreferencesRead: ['GET', '/dj/preferences'], djPreferencesWrite: ['POST', '/dj/preferences'],
  });
});

test('all fourteen list keys support one-value addition and removal', () => {
  assert.equal(prefs.listKeys.length, 14);
  for (const key of prefs.listKeys) {
    assert.deepEqual(prefs.changeOf(key, '  Earth, Wind & Fire  '), { key, value: 'Earth, Wind & Fire', unset: false });
    assert.deepEqual(prefs.changeOf(key, 'Earth, Wind & Fire', true), { key, value: 'Earth, Wind & Fire', unset: true });
  }
});

test('list removal cannot accidentally clear an entire preference list', () => {
  for (const key of prefs.listKeys) for (const value of [null, undefined, '', ' ', ['jazz']]) {
    assert.throws(() => prefs.changeOf(key, value, true));
  }
});

test('energy includes zero and one hundred, and reset is an explicit unset', () => {
  for (let energy = 0; energy <= 100; energy++) {
    assert.deepEqual(prefs.changeOf('energy', String(energy)), { key: 'energy', value: String(energy), unset: false });
  }
  assert.deepEqual(prefs.changeOf('energy', null, true), { key: 'energy', unset: true });
  assert.deepEqual(prefs.changeOf('energy', '001'), { key: 'energy', value: '1', unset: false });
  for (const value of ['-1', '101', '1.5', '1e2', 'NaN', '', 'true']) assert.throws(() => prefs.changeOf('energy', value));
});

test('explicit tracks require a boolean choice, not a truthy string', () => {
  assert.deepEqual(prefs.changeOf('explicit', 'false'), { key: 'explicit', value: 'false', unset: false });
  assert.deepEqual(prefs.changeOf('explicit', 'true'), { key: 'explicit', value: 'true', unset: false });
  assert.deepEqual(prefs.changeOf('explicit', null, true), { key: 'explicit', unset: true });
  for (const value of ['yes', '0', 'TRUE', true, false]) assert.throws(() => prefs.changeOf('explicit', value));
});

test('value bounds count Unicode characters rather than UTF-16 units', () => {
  assert.equal(prefs.changeOf('favor.artists', '🎵'.repeat(256)).value, '🎵'.repeat(256));
  assert.throws(() => prefs.changeOf('favor.artists', '🎵'.repeat(257)));
  assert.throws(() => prefs.changeOf('constructor', 'jazz'));
  assert.throws(() => prefs.changeOf('favor.genres', 'jazz', 'false'));
});

test('snapshots are copied and deeply frozen; literal names and duplicates survive', () => {
  const raw = defaults();
  raw.favor.artists = ['<img src=x onerror=alert(1)>', 'A', 'A'];
  raw.futureSetting = { keepMe: true };
  const observed = prefs.preferencesOf(raw);
  raw.favor.artists.push('Later');
  assert.equal(observed.favor.artists.length, 3);
  assert.throws(() => observed.favor.artists.push('Changed'));
  assert.throws(() => { observed.energy = 50; });
  assert.equal(observed.favor.artists[0], '<img src=x onerror=alert(1)>');
});

test('malformed preference reads never become editable defaults', () => {
  const variants = [null, {}, { ...defaults(), explicit: 'false' }, { ...defaults(), energy: 101 },
    { ...defaults(), energy: -1 }, { ...defaults(), energy: 1.1 }, { ...defaults(), favor: {} }];
  for (const mutate of [
    (p) => { p.ban.artists = null; }, (p) => { p.favor.genres = [17]; },
    (p) => { p.pin.tracks = ['']; }, (p) => { p.avoid.eras = ['x'.repeat(257)]; },
    (p) => { p.favor.genres = Array(5001).fill('jazz'); },
  ]) { const value = defaults(); mutate(value); variants.push(value); }
  for (const value of variants) assert.throws(() => prefs.preferencesOf(value));
});

test('opening, reviewing, cancelling and closing are read-only', async () => {
  const { editor, calls } = fixture();
  assert.equal(editor.review('energy', '70'), null);
  await editor.load();
  const review = editor.review('favor.genres', 'jazz');
  assert.equal(review.body.value, 'jazz');
  editor.cancel();
  assert.equal(await editor.confirm(review), false);
  editor.close();
  assert.equal(calls.length, 1);
  assert.equal(calls[0][0], 'djPreferencesRead');
});

test('confirmation sends only the one reviewed key, never the whole snapshot', async () => {
  const { editor, calls, writes } = fixture();
  const answer = defaults(); answer.favor.genres = ['jazz']; answer.ban.artists = ['Another artist'];
  writes.push(saved(answer));
  await editor.load();
  const pending = editor.review('favor.genres', 'jazz');
  assert.ok(Object.isFrozen(pending) && Object.isFrozen(pending.body));
  assert.equal(await editor.confirm(pending), true);
  assert.deepEqual(calls[1], ['djPreferencesWrite', { body: { key: 'favor.genres', value: 'jazz', unset: false } }]);
  assert.deepEqual(editor.state.preferences.ban.artists, ['Another artist']);
  assert.equal(editor.state.message, 'Preference saved.');
});

test('a newer review invalidates old confirmation buttons', async () => {
  const { editor, calls } = fixture();
  await editor.load();
  const old = editor.review('favor.genres', 'jazz');
  const current = editor.review('favor.genres', 'soul');
  assert.equal(await editor.confirm(old), false);
  assert.equal(await editor.confirm({ ...current }), false);
  assert.equal(await editor.confirm(current), true);
  assert.equal(calls.filter(([name]) => name === 'djPreferencesWrite').length, 1);
});

test('double clicks and other edits cannot dispatch while a save is pending', async () => {
  const { editor, calls, writes } = fixture();
  const wait = deferred(); writes.push(wait.promise);
  await editor.load();
  const pending = editor.review('energy', '50');
  const first = editor.confirm(pending);
  assert.equal(await editor.confirm(pending), false);
  assert.equal(editor.review('energy', '60'), null);
  assert.equal(await editor.load(), false);
  assert.equal(calls.length, 2);
  wait.resolve(saved({ ...defaults(), energy: 50 }));
  assert.equal(await first, true);
});

test('read generations ignore a slower superseded response', async () => {
  const { editor, reads } = fixture();
  const slow = deferred(); reads.push(slow.promise, { ...defaults(), energy: 70 });
  const first = editor.load();
  await editor.load();
  slow.resolve({ ...defaults(), energy: 10 });
  assert.equal(await first, false);
  assert.equal(editor.state.preferences.energy, 70);
});

test('a refresh invalidates all unsent reviews', async () => {
  const { editor, calls } = fixture();
  await editor.load();
  const old = editor.review('ban.artists', 'Example');
  await editor.load();
  assert.equal(await editor.confirm(old), false);
  assert.equal(calls.length, 2);
});

test('closing during a read ignores its completion and cannot send a stale review', async () => {
  const { editor, reads, calls } = fixture();
  const slow = deferred(); reads.push(slow.promise);
  const loading = editor.load(); editor.close(); slow.resolve(defaults());
  assert.equal(await loading, false);
  assert.equal(editor.state.ready, false);
  assert.equal(editor.state.open, false);
  assert.equal(editor.state.loading, false);
  assert.equal(calls.length, 1);
});

test('closing and reopening an in-flight save never replays or hides its ownership', async () => {
  const { editor, writes, calls } = fixture();
  const slow = deferred(); writes.push(slow.promise);
  await editor.load();
  const pending = editor.review('energy', '40');
  const saving = editor.confirm(pending); editor.close();
  assert.equal(editor.state.writing, true);
  assert.equal(await editor.load(), false);
  assert.equal(editor.state.open, true);
  slow.resolve(saved({ ...defaults(), energy: 40 }));
  assert.equal(await saving, true);
  assert.equal(editor.state.preferences.energy, 40);
  assert.equal(calls.length, 2);
});

test('failed saves expose the remedy, require a fresh read, and never retry', async () => {
  const { editor, writes, calls } = fixture();
  const failed = deferred(); writes.push(failed.promise);
  await editor.load();
  const pending = editor.review('energy', '40');
  const saving = editor.confirm(pending);
  failed.reject(Object.assign(new Error('Request failed.'), { hint: 'Check the daemon.' }));
  assert.equal(await saving, false);
  assert.match(editor.state.message, /Check the daemon.*Refresh preferences/);
  assert.equal(editor.state.ready, false);
  assert.equal(editor.review('energy', '40'), null);
  assert.equal(await editor.confirm(pending), false);
  assert.equal(calls.length, 2);
  await editor.load();
  assert.equal(editor.state.ready, true);
});

test('successful HTTP with an incomplete save reply is still an uncertain outcome', async () => {
  for (const response of [null, {}, { changed: true, done: 'Saved' }, { changed: 'true', done: 'Saved', preferences: defaults() }]) {
    const { editor, writes } = fixture(); writes.push(response);
    await editor.load();
    assert.equal(await editor.confirm(editor.review('energy', '1')), false);
    assert.equal(editor.state.ready, false);
    assert.match(editor.state.message, /do not resend/);
  }
});

test('failed refresh keeps old observations read-only rather than overwriting settings', async () => {
  const { editor, reads } = fixture();
  await editor.load();
  reads.push({});
  assert.equal(await editor.load(), false);
  assert.equal(editor.state.ready, false);
  assert.ok(editor.state.preferences);
  assert.equal(editor.review('energy', '40'), null);
});

test('disposal discards asynchronous publication and never initiates another request', async () => {
  const wait = deferred(); let changes = 0, calls = 0;
  const editor = prefs.model({ call: () => { calls++; return wait.promise; }, changed: () => changes++ });
  const loading = editor.load(); editor.dispose(); const before = changes;
  wait.resolve(defaults()); await loading;
  assert.equal(changes, before);
  assert.equal(await editor.load(), false);
  assert.equal(calls, 1);
});

test('the shipped route map dispatches an exact JSON preference over a real loopback socket', async (t) => {
  const received = [];
  const current = defaults();
  const server = http.createServer(async (request, response) => {
    let body = ''; for await (const part of request) body += part;
    received.push({ method: request.method, path: request.url, contentType: request.headers['content-type'], body });
    response.setHeader('Content-Type', 'application/json');
    if (request.method === 'POST') {
      const change = JSON.parse(body); current.favor.artists.push(change.value);
      response.end(JSON.stringify(saved(current)));
    } else response.end(JSON.stringify(current));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); return new Promise((resolve) => server.close(resolve)); });
  const editor = prefs.model({ call: async (name, { body } = {}) => {
    const [method, route] = prefs.routes[name];
    const response = await fetch(`http://127.0.0.1:${server.address().port}${route}`, {
      method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined,
    });
    return response.json();
  } });
  await editor.load();
  await editor.confirm(editor.review('favor.artists', 'Earth, Wind & Fire'));
  assert.deepEqual(received.map((r) => [r.method, r.path]), [['GET', '/dj/preferences'], ['POST', '/dj/preferences']]);
  assert.equal(received[1].contentType, 'application/json');
  assert.deepEqual(JSON.parse(received[1].body), { key: 'favor.artists', value: 'Earth, Wind & Fire', unset: false });
  assert.deepEqual(editor.state.preferences.favor.artists, ['Earth, Wind & Fire']);
});
