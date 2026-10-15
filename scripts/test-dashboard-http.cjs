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
