'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const z = { household: 'S2', coordinator_room: 'Synthetic room', members: ['Synthetic room'], transport_state: 'playing' };
async function household(t, behavior = 'ok') {
  const calls = [];
  const state = { zone: z, transport_state: 'playing', volume: 25, track: { uri: 'synthetic:song', position_secs: 10, duration_secs: 120 } };
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    calls.push({ url: req.url, method: req.method, body: raw ? JSON.parse(raw) : null });
    res.setHeader('Content-Type', 'application/json');
    if (req.url === '/zones') { res.end(JSON.stringify([z])); return; }
    if (req.url.endsWith('/state')) { res.end(JSON.stringify(state)); return; }
    if (req.method === 'POST') {
      state.transport_state = 'paused';
      if (behavior === 'lost') { req.socket.destroy(); return; }
      if (behavior === 'malformed') { res.end('{broken'); return; }
      if (behavior === 'policy') { res.statusCode = 403; res.end(JSON.stringify({ detail: 'Denied', hint: 'Ask the owner.' })); return; }
      res.end(JSON.stringify({ done: 'Paused synthetic room', changed: true })); return;
    }
    res.statusCode = 404; res.end('{}');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = C.request({ fetcher: (url, init) => fetch(base + url, init) });
  const model = C.model({ call }); await model.refresh();
  return { model, calls, state };
}
test('real loopback HTTP: a production pause request targets the qualified room once', async (t) => {
  const { model, calls } = await household(t);
  assert.equal(await model.command('pause'), true);
  assert.deepEqual(calls.filter((r) => r.method === 'POST'), [{ url: '/pause', method: 'POST', body: { zone: 'Synthetic room@S2' } }]);
  assert.equal(model.selected().state.transport_state, 'paused');
});
test('real loopback HTTP: a lost reply after application is not replayed', async (t) => {
  const { model, calls } = await household(t, 'lost');
  assert.equal(await model.command('pause'), false); assert.equal(model.state.uncertain, true);
  await model.refresh(); assert.equal(model.canControl(), false); assert.equal(model.selected().state.transport_state, 'paused');
  assert.equal(calls.filter((r) => r.method === 'POST').length, 1);
  await model.refresh(true); assert.equal(model.canControl(), true);
});
test('real loopback HTTP: malformed acknowledgements require explicit refresh', async (t) => {
  const { model, calls } = await household(t, 'malformed');
  assert.equal(await model.command('pause'), false); assert.equal(model.state.uncertain, true);
  assert.equal(await model.command('pause'), false); assert.equal(calls.filter((r) => r.method === 'POST').length, 1);
});
test('real loopback HTTP: changed household membership prevents any POST', async (t) => {
  const { model, calls, state } = await household(t);
  state.zone = { ...z, members: [z.coordinator_room, 'New member'] };
  assert.equal(await model.command('pause'), false); assert.equal(calls.filter((r) => r.method === 'POST').length, 0);
});
test('real loopback HTTP: policy denials retain their remedy without retrying', async (t) => {
  const { model, calls } = await household(t, 'policy');
  assert.equal(await model.command('pause'), false); assert.match(model.state.message, /Ask the owner/);
  assert.equal(calls.filter((r) => r.method === 'POST').length, 1);
});
test('a throttled read preserves the complete Retry-After interval', async () => {
  const call = C.request({ fetcher: async () => new Response(JSON.stringify({ detail: 'Busy' }), { status: 429, headers: { 'Retry-After': '180' } }) });
  await assert.rejects(call('/zones'), (e) => e.retryAfter === 180000);
});
test('dashboard routes embed the real assets and retain the existing browser guards', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const mount = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  const module = fs.readFileSync(path.join(root, 'http/remote/dashboard.rs'), 'utf8');
  assert.match(mount, /routes\.extend\(dashboard::routes\(cx\)\)/);
  for (const name of ['dashboard.html', 'dashboard.css', 'dashboard-core.js', 'dashboard-visualizer.js', 'dashboard-audio.js', 'dashboard.js']) {
    assert.ok(module.includes(name), name); assert.ok(fs.statSync(path.join(root, 'remote', name)).size > 0);
  }
  assert.match(module, /asset\(content_type, text\)/); assert.match(module, /microphone=\(self\)/);
  assert.match(mount, /frame-ancestors 'none'/); assert.match(mount, /connect-src 'self'/);
});
test('production assets have no inline scripts, external dependencies or unsafe metadata sinks', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src/remote');
  const html = fs.readFileSync(path.join(root, 'dashboard.html'), 'utf8');
  assert.doesNotMatch(html, /<script(?![^>]*src=)[^>]*>|on(?:click|load|error)=/i);
  for (const file of ['dashboard.js', 'dashboard-core.js', 'dashboard-visualizer.js', 'dashboard-audio.js']) {
    const source = fs.readFileSync(path.join(root, file), 'utf8');
    assert.doesNotMatch(source.replaceAll('http://www.w3.org/2000/svg', ''), /innerHTML|outerHTML|insertAdjacentHTML|eval\(|https?:\/\//);
  }
  assert.match(html, /id="demo-banner"/); assert.match(html, /aria-label="Visualization style"/);
});

test('native PUT requests retain JSON, same-origin, no-redirect and single-dispatch behavior', async () => {
  const calls = [], call = C.request({ fetcher: async (url, init) => {
    calls.push({ url, init }); return { ok: true, text: async () => '{"name":"Evening"}' };
  } });
  assert.deepEqual(await call('/scenes/Evening', { backup_queues: true }, { method: 'PUT' }), { name: 'Evening' });
  assert.equal(calls.length, 1); assert.equal(calls[0].init.method, 'PUT');
  assert.deepEqual(JSON.parse(calls[0].init.body), { backup_queues: true });
  assert.equal(calls[0].init.headers['Content-Type'], 'application/json'); assert.equal(calls[0].init.headers.Accept, 'application/json');
  assert.equal(calls[0].init.credentials, 'same-origin'); assert.equal(calls[0].init.redirect, 'error'); assert.equal(calls[0].init.cache, 'no-store');
});

test('method/body mismatches and unsupported methods never reach fetch', async () => {
  let requests = 0; const call = C.request({ fetcher: async () => { requests++; throw new Error('Unexpected fetch'); } });
  for (const [body, method] of [[{}, 'GET'], [undefined, 'PUT'], [undefined, 'POST'], [{}, 'DELETE'], [{}, 'put']]) {
    await assert.rejects(call('/scenes/Evening', body, { method }), /request method does not match/);
  }
  assert.equal(requests, 0);
});

test('queue-search reads have a bounded scan deadline while ordinary reads retain their short budget', async () => {
  const budgets = [], calls = [];
  const call = C.request({ schedule(fn, ms) { budgets.push(ms); return budgets.length; }, cancel() {},
    fetcher: async (url, init) => { calls.push({ url, method: init.method }); return { ok: true, text: async () => '{}' }; } });
  const cases = [
    ['/zones', undefined, undefined, 10000],
    ['/zones/Studio%40S2/queue?offset=0&limit=40', undefined, undefined, 10000],
    ['/zones/Studio%40S2/queue/search?q=fixture&limit=20', undefined, undefined, 60000],
    ['/zones/Studio%40S2/queue/search', undefined, undefined, 60000],
    ['/zones/Studio%40S2/queue/search-more?q=fixture', undefined, undefined, 10000],
    ['/zones/Studio%40S2/state?next=/queue/search', undefined, undefined, 10000],
    ['/scenes/Evening', {}, { method: 'PUT' }, 120000],
    ['/scenes/Evening', { backup_queues: true }, { method: 'PUT' }, 600000],
    ['/scenes/Evening', null, { method: 'PUT' }, 120000],
    ['/pause', { zone: 'Studio@S2' }, undefined, 120000],
  ];
  for (const [url, body, options] of cases) await call(url, body, options);
  assert.deepEqual(budgets, cases.map(row => row[3]));
  assert.deepEqual(calls.map(row => row.method), ['GET', 'GET', 'GET', 'GET', 'GET', 'GET', 'PUT', 'PUT', 'PUT', 'POST']);
});

test('a queue-search deadline aborts the outstanding read without a retry', async () => {
  let expire, signal, requests = 0;
  const call = C.request({ schedule(fn, ms) { assert.equal(ms, 60000); expire = fn; return 1; }, cancel() {},
    fetcher: async (url, init) => { requests++; signal = init.signal; return new Promise(() => {}); } });
  const reading = call('/zones/Studio%40S2/queue/search?q=fixture');
  const rejected = assert.rejects(reading, /did not answer in time/);
  expire(); await rejected; assert.equal(signal.aborted, true); assert.equal(requests, 1);
});

test('cancelReads retires observations but never aborts or repeats an already submitted PUT', async () => {
  let finish, writeSignal, readSignal;
  const calls = [], call = C.request({ schedule() { return 1; }, cancel() {}, fetcher: async (url, init) => {
    calls.push(init.method);
    if (init.method === 'PUT') { writeSignal = init.signal; return new Promise(resolve => { finish = resolve; }); }
    readSignal = init.signal; return new Promise(() => {});
  } });
  const reading = call('/zones'), rejected = assert.rejects(reading, error => error.code === 'DASHBOARD_READ_CANCELLED');
  const saving = call('/scenes/Evening', {}, { method: 'PUT' }); call.cancelReads(); await rejected;
  assert.equal(readSignal.aborted, true); assert.equal(writeSignal.aborted, false);
  finish({ ok: true, text: async () => '{"name":"Evening"}' }); assert.deepEqual(await saving, { name: 'Evening' });
  assert.deepEqual(calls, ['GET', 'PUT']);
});

test('only native announcement POST receives the synthesis, playback and restoration deadline', async () => {
  const budgets = [], call = C.request({ schedule(fn, ms) { budgets.push(ms); return 1; }, cancel() {},
    fetcher: async () => ({ ok: true, text: async () => '{}' }) });
  await call('/announce', { text: 'Synthetic notice' });
  await call('/announce', undefined);
  await call('/announce', {}, { method: 'PUT' });
  await call('/announce-more', {});
  await call('/schedules', {});
  assert.deepEqual(budgets, [1200000, 10000, 120000, 120000, 120000]);
});
