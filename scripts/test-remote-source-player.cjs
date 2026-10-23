// node --test scripts/test-remote-source-player.cjs; shipped controller, no speaker access.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { once } = require('node:events');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const Player = require('../crates/fsonos-api/src/remote/source-player.js');
const ID = '0123456789ABCDEFGHIJKL';
assert.equal(ID.length, 22);
const zone = (home = 'S2', name = 'Den', members = [name]) => ({ household: home, coordinator_room: name, members });
const state = (z = zone(), overrides = {}) => ({ zone: z, transport_state: 'playing',
  track: { uri: 'sample:current', title: 'Current song', queue_position: 3, position_secs: 20 }, ...overrides });
const defer = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };

for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) test(`Spotify ${kind} links canonicalize without contacting Spotify`, () => {
  for (const uri of [`spotify:${kind}:${ID}`, ` Spotify:${kind.toUpperCase()}:${ID} `,
    `https://open.spotify.com/${kind}/${ID}?si=tracking#ignored`,
    `https://OPEN.SPOTIFY.COM/intl-fr/embed/${kind}/${ID}/`, `http://play.spotify.com/${kind}/${ID}`,
    `https://open.spotify.com.:443/${kind}/${ID}`]) {
    const parsed = Player.source(uri);
    assert.equal(parsed.uri, `spotify:${kind}:${ID}`); assert.equal(parsed.container, kind === 'album' || kind === 'playlist'); assert.ok(Object.isFrozen(parsed));
  }
});

test('legacy Spotify playlist URIs and share links keep the playlist id', () => {
  for (const uri of [`spotify:user:someone:playlist:${ID}`, `https://open.spotify.com/user/someone/playlist/${ID}`]) {
    assert.equal(Player.source(uri).uri, `spotify:playlist:${ID}`);
  }
});

test('malformed, unsupported and shortened music addresses fail before any I/O', () => {
  for (const uri of ['', null, 'a song title', 'spotify:track:short', `spotify:album:${ID}x`,
    `spotify:track:${ID.slice(1)}_`, `spotify:genre:${ID}`, `spotify:user:${ID}`, `spotify:local:${ID}`,
    'https://open.spotify.com/', 'https://spotify.link/abc', 'https://a.spotify.link/abc',
    'custom:source', 'ftp://audio.example/track',
    'https://name@audio.example/music', 'https://audio.example:99999/live',
    'https://audio.example/white space', 'https://audio.example/\u0000live', 'https:\\audio.example\\live']) {
    assert.throws(() => Player.source(uri), undefined, String(uri));
  }
});

test('stream addresses and signed query bytes are never fetched, decoded or rewritten', () => {
  for (const uri of ['https://audio.example/Live%2fTrack.mp3?Signature=a%2Bb&part=1',
    'http://media.example:8080/audio.aac', 'http://[::1]:8080/live']) {
    assert.deepEqual(Player.source(uri), { uri, kind: 'stream', container: false });
  }
});

test('source limits count UTF-8 bytes, not JavaScript code units', () => {
  const prefix = 'https://audio.example/';
  assert.doesNotThrow(() => Player.source(prefix + 'x'.repeat(2048 - prefix.length)));
  assert.throws(() => Player.source(prefix + 'x'.repeat(2049 - prefix.length)));
  assert.throws(() => Player.source(prefix + 'é'.repeat(1024)));
});

test('groups distinguish households and reject missing or contradictory membership', () => {
  assert.deepEqual(Player.groups([zone('S1'), zone('S2')]).map(g => g.target), ['Den@S1', 'Den@S2']);
  const a = zone('S2', 'Den', ['Den', 'Office']);
  assert.equal(Player.groups([a])[0].signature, Player.groups([{ ...a, members: ['Office', 'Den'] }])[0].signature);
  for (const value of [null, {}, [zone('S2', 'Den', [])], [zone('S2', 'Den', ['Office'])],
    [zone('S2', 'Den', ['Den', 'Den'])], [zone(), zone()], [zone('S2', 'x'.repeat(129))]]) assert.throws(() => Player.groups(value));
});

function harness(options = {}) {
  const h = { zones: [zone()], now: state(), calls: [], custom: null, changes: 0, executions: 0 };
  h.api = Player.model({
    call: async (name, args) => {
      h.calls.push([name, args]);
      if (h.custom) { const answer = h.custom(name, args); if (answer !== undefined) return answer; }
      if (name === 'zones') return structuredClone(h.zones);
      if (name === 'state') return structuredClone(h.now);
      if (name === 'sourcePlay') return { done: 'Playback accepted', changed: true, notes: [{ detail: 'Volume capped.' }] };
      throw new Error(`Unexpected call ${name}`);
    },
    changed: () => { h.changes++; }, execute: async (_, work) => { h.executions++; return work(); }, ...options,
  });
  h.writes = () => h.calls.filter(([name]) => name === 'sourcePlay');
  h.open = async (input = `spotify:track:${ID}`) => {
    await h.api.refresh(); h.api.edit('target', `${h.zones[0].coordinator_room}@${h.zones[0].household}`); h.api.edit('input', input);
  };
  h.reviewed = async (input) => { await h.open(input); assert.equal(await h.api.review(), true); return h.api.state.pending; };
  return h;
}

for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) test(`${kind} playback requires a destination and one confirmed policy-path request`, async () => {
  const h = harness();
  await h.api.refresh(); h.api.edit('input', `https://open.spotify.com/${kind}/${ID}?si=anything`);
  assert.equal(h.api.state.target, '');
  assert.equal(await h.api.review(), false); assert.equal(h.writes().length, 0);
  h.api.edit('target', 'Den@S2'); assert.equal(await h.api.review(), true);
  const pending = h.api.state.pending;
  assert.equal(h.writes().length, 0);
  assert.equal(pending.item.container, kind === 'album' || kind === 'playlist');
  assert.equal(await h.api.confirm(pending), true); assert.equal(await h.api.confirm(pending), false);
  assert.deepEqual(h.writes(), [['sourcePlay', { body: { zone: 'Den@S2', source_uri: `spotify:${kind}:${ID}` } }]]);
  assert.equal(h.api.state.result.status, 'accepted'); assert.match(h.api.state.result.detail, /Volume capped/);
  assert.equal(h.api.state.writing, false); assert.equal(h.api.state.pending, null);
});

test('streams work without Spotify or a synced library and carry an optional trimmed title', async () => {
  const h = harness(); await h.open('https://audio.example/music.mp3');
  h.api.edit('title', '  Evening radio  '); await h.api.review();
  assert.equal(await h.api.confirm(h.api.state.pending), true);
  assert.deepEqual(h.writes()[0][1].body, { zone: 'Den@S2', source_uri: 'https://audio.example/music.mp3', title: 'Evening radio' });
  assert.ok(h.calls.every(([name]) => ['zones', 'state', 'sourcePlay'].includes(name)));
});

test('invalid sources do not even query the destination and titles use the native character limit', async () => {
  const h = harness(); await h.open('bad input'); const n = h.calls.length;
  assert.equal(await h.api.review(), false); assert.equal(h.calls.length, n);
  h.api.edit('input', `spotify:track:${ID}`); h.api.edit('title', '🎵'.repeat(300));
  await h.api.review(); assert.equal([...h.api.state.pending.body.title].length, 256);
  h.api.edit('title', 'bad\u0001name'); assert.equal(await h.api.review(), false);
});

test('review snapshot is immutable and position, volume and cosmetic changes are not playback replacements', async () => {
  const h = harness(), p = await h.reviewed(); assert.ok(Object.isFrozen(p) && Object.isFrozen(p.body));
  h.now.track.position_secs = 40; h.now.track.title = 'New metadata'; h.now.volume = 30;
  assert.equal(await h.api.confirm(p), true);
});

test('changed URI, duplicate queue slot, play state or group causes no playback command', async () => {
  for (const change of [h => { h.now.track.uri = 'sample:other'; }, h => { h.now.track.queue_position++; },
    h => { h.now.transport_state = 'paused'; }, h => { h.zones = [zone('S2', 'Den', ['Den', 'Other'])]; },
    h => { h.now.zone = zone('S1'); }]) {
    const h = harness(), p = await h.reviewed(); change(h);
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
    assert.equal(h.api.state.result.status, 'not_sent');
  }
});

test('missing tracks can be deliberately replaced but malformed state cannot be reviewed', async () => {
  const h = harness(); h.now.track = null; const p = await h.reviewed();
  assert.match(p.before.label, /No current track/); assert.equal(await h.api.confirm(p), true);
  for (const value of [null, {}, state(zone(), { track: {} }), state(zone(), { transport_state: 'bogus' })]) {
    const h = harness(); h.now = value; await h.open(); assert.equal(await h.api.review(), false); assert.equal(h.writes().length, 0);
  }
});

test('changing any draft field, canceling or closing consumes old confirmation handlers', async () => {
  for (const change of [h => h.api.edit('input', `spotify:album:${ID}`), h => h.api.edit('title', 'New'),
    h => h.api.edit('target', 'Den@S2'), h => h.api.cancel(), h => h.api.close()]) {
    const h = harness(), p = await h.reviewed(); change(h);
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
  }
  const h = harness(), p = await h.reviewed(); assert.equal(await h.api.confirm(structuredClone(p)), false);
});

test('slow reviews are fenced by newer drafts or close/reopen', async () => {
  const h = harness(), wait = defer(); await h.open();
  h.custom = name => name === 'state' ? wait.promise : undefined;
  const pending = h.api.review(); await settle(); h.api.edit('input', `spotify:album:${ID}`);
  wait.resolve(h.now); assert.equal(await pending, false); assert.equal(h.api.state.pending, null);
  h.api.close(); await h.api.refresh(); assert.equal(h.api.state.pending, null);
});

test('empty and failed discovery never leave an old destination actionable', async () => {
  const h = harness(); await h.reviewed(); h.zones = []; await h.api.refresh();
  assert.equal(h.api.state.target, ''); assert.equal(h.api.state.pending, null); assert.match(h.api.state.message, /No groups/);
  h.custom = () => { throw new Error('offline'); }; assert.equal(await h.api.refresh(), false);
  assert.equal(h.api.state.groups.length, 0); assert.equal(h.writes().length, 0);
});

test('double clicks, editing and reopening do not overlap an in-flight playback request', async () => {
  const h = harness(), p = await h.reviewed(), wait = defer(); h.custom = name => name === 'sourcePlay' ? wait.promise : undefined;
  const action = h.api.confirm(p); await settle();
  assert.equal(await h.api.confirm(p), false); assert.equal(h.api.edit('input', 'new'), false);
  h.api.close(); assert.equal(await h.api.refresh(), false); assert.equal(h.api.state.writing, true);
  wait.resolve({ done: 'Playing', changed: true }); assert.equal(await action, true);
  assert.equal(h.writes().length, 1); assert.equal(h.api.state.result.status, 'accepted');
});

test('closing during preflight releases the action lane without sending anything', async () => {
  const h = harness(), p = await h.reviewed(), wait = defer(); h.custom = name => name === 'state' ? wait.promise : undefined;
  const action = h.api.confirm(p); await settle(); h.api.close(); wait.resolve(h.now);
  assert.equal(await action, false); assert.equal(h.writes().length, 0); assert.equal(h.api.state.writing, false);
});

test('lost and malformed replies remain uncertain and are never replayed on reopening', async () => {
  for (const response of [null, {}, { done: 'ok' }, { done: 'ok', changed: true, notes: [null] }, new Error('reply lost')]) {
    const h = harness(), p = await h.reviewed(); h.custom = name => {
      if (name !== 'sourcePlay') return undefined;
      if (response instanceof Error) throw response;
      return response;
    };
    assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain');
    h.api.close(); await h.api.refresh();
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 1);
    assert.match(h.api.state.result.detail, /not repeated/);
  }
});

test('the native action lane can reject a busy group without a fallback write', async () => {
  const h = harness({ execute: async () => { throw new Error('another playback action is busy'); } });
  const p = await h.reviewed(); assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 0);
  assert.equal(h.api.state.result.status, 'not_sent');
});

test('a denied playback command retains its remedy and does not repeat', async () => {
  const h = harness(), p = await h.reviewed(); h.custom = name => {
    if (name === 'sourcePlay') throw Object.assign(new Error('Not allowed'), { code: 'POLICY_DENIED', hint: 'Ask the owner to allow play.' });
  };
  await h.api.confirm(p); assert.match(h.api.state.result.detail, /Ask the owner/); assert.equal(h.writes().length, 1);
});

test('observer exceptions cannot change the operation result and disposal prevents new work', async () => {
  const h = harness({ changed() { throw new Error('view'); } });
  const p = await h.reviewed(); assert.equal(await h.api.confirm(p), true); h.api.dispose();
  assert.equal(await h.api.refresh(), false); assert.equal(h.api.edit('input', 'x'), false); assert.equal(await h.api.confirm(p), false);
});

class Element {
  constructor(tag = '') { this.tag = tag; this.children = []; this.events = new Map(); this.attributes = {}; this.textContent = ''; this.value = ''; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  setAttribute(key, value) { this.attributes[key] = String(value); if (['value', 'disabled', 'hidden'].includes(key)) this[key] = value; }
  addEventListener(key, fn) { this.events.set(key, fn); }
  removeEventListener(key) { this.events.delete(key); }
  emit(key) { return this.events.get(key)?.(); }
}
const descendants = n => [n, ...(n.children || []).flatMap(v => typeof v === 'object' ? descendants(v) : [])];
function panelFixture(call) {
  const doc = new Element(); doc.hidden = false;
  const el = (tag, attrs = {}, ...children) => {
    const node = new Element(tag); Object.assign(node.attributes, attrs);
    for (const [k, v] of Object.entries(attrs)) { if (k === 'text') node.textContent = v; else node[k] = v; }
    node.append(...children); return node;
  };
  const panel = Player.panel({ call, el, document: doc });
  return { doc, panel, el, find: text => descendants(panel.node).find(n => n.tag === 'button' && n.textContent === text),
    field: label => descendants(panel.node).find(n => n.attributes['aria-label'] === label) };
}

test('the real panel preserves text inputs and sends only its current explicit review', async () => {
  const h = harness(), ui = panelFixture(h.api ? async (name, args) => {
    h.calls.push([name, args]); return name === 'zones' ? h.zones : name === 'state' ? h.now : { done: 'Playing', changed: true };
  } : null);
  await ui.panel.refresh(); ui.panel.edit('target', 'Den@S2');
  const input = ui.field('Spotify link or audio stream address'); input.value = `spotify:track:${ID}`; input.emit('input');
  const original = input; await ui.panel.review();
  assert.equal(ui.field('Spotify link or audio stream address'), original);
  const old = ui.find('Play now'); assert.ok(old); assert.equal(h.writes().length, 0);
  ui.panel.edit('title', '<b>Untrusted title</b>'); old.emit('click'); await settle(); assert.equal(h.writes().length, 0);
  await ui.panel.review(); ui.find('Play now').emit('click'); await settle();
  assert.equal(h.writes().length, 1); assert.equal(h.writes()[0][1].body.title, '<b>Untrusted title</b>');
  assert.ok(!fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/source-player.js'), 'utf8').includes('innerHTML'));
  ui.panel.dispose(); assert.equal(ui.doc.events.size, 0);
});

test('hidden pages cannot preserve an actionable review', async () => {
  const ui = panelFixture(async name => name === 'zones' ? [zone()] : state());
  await ui.panel.refresh(); ui.panel.edit('target', 'Den@S2'); ui.panel.edit('input', `spotify:track:${ID}`); await ui.panel.review();
  const p = ui.panel.state.pending; ui.doc.hidden = true; ui.doc.emit('visibilitychange');
  assert.equal(ui.panel.state.pending, null); assert.equal(await ui.panel.confirm(p), false); ui.panel.dispose();
});

test('actual loopback HTTP receives the qualified canonical source exactly once', async (t) => {
  const z = zone('S1', 'A & B/?#'), received = [];
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null;
    received.push([req.method, req.url, body]); res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(req.url === '/zones' ? [z] : req.url === '/play' ? { done: 'Playing', changed: true } : state(z)));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const root = `http://127.0.0.1:${server.address().port}`;
  const api = Player.model({ call: async (name, args = {}) => {
    const url = name === 'zones' ? '/zones' : name === 'state' ? `/zones/${encodeURIComponent(args.params.room)}/state` : '/play';
    const init = name === 'sourcePlay' ? { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(args.body) } : {};
    const response = await fetch(root + url, init); return response.json();
  } });
  await api.refresh(); api.edit('target', 'A & B/?#@S1'); api.edit('input', `https://open.spotify.com/intl-de/track/${ID}?si=abc`);
  await api.review(); assert.equal(await api.confirm(api.state.pending), true);
  assert.deepEqual(received.filter(([m]) => m === 'POST'), [['POST', '/play', { zone: 'A & B/?#@S1', source_uri: `spotify:track:${ID}` }]]);
  assert.ok(received.filter(([, p]) => p.endsWith('/state')).every(([, p]) => p === '/zones/A%20%26%20B%2F%3F%23%40S1/state'));
});

test('the browser adapter uses the existing native action lock and releases it after failures', async () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/source-player.js'), 'utf8');
  const h = harness(); let inserted, begins = 0, ends = 0, busy = false;
  const doc = new Element(); doc.hidden = false; doc.getElementById = () => ({ before: node => { inserted = node; } });
  const group = Player.groups([zone()])[0], parts = { zone: zone() };
  const ui = panelFixture(async () => []);
  const ctx = { URL, TextEncoder, document: doc, ROUTES: {}, house: { cards: new Map([['room', parts]]) },
    groupShape: () => group.signature, loadZones: async () => {}, el: ui.el,
    call: async (name, args) => { h.calls.push([name, args]); return name === 'zones' ? [zone()] : name === 'state' ? state() : { done: 'Playing', changed: true }; },
    beginAction: p => { assert.equal(p, parts); begins++; return !busy; }, endAction: p => { assert.equal(p, parts); ends++; },
  };
  vm.runInNewContext(source, ctx); assert.ok(inserted); assert.deepEqual(Array.from(ctx.ROUTES.sourcePlay), ['POST', '/play']);
  const details = descendants(inserted).find(n => n.tag === 'details'); details.open = true; details.emit('toggle'); await settle();
  const target = descendants(inserted).find(n => n.tag === 'select'); target.value = 'Den@S2'; target.emit('change');
  const input = descendants(inserted).find(n => n.attributes['aria-label'] === 'Spotify link or audio stream address'); input.value = `spotify:track:${ID}`; input.emit('input');
  const button = label => descendants(inserted).find(n => n.tag === 'button' && n.textContent === label);
  button('Review playback').emit('click'); await settle(); busy = true; button('Play now').emit('click'); await settle();
  assert.equal(h.writes().length, 0); assert.equal(ends, 0);
  busy = false; button('Review playback').emit('click'); await settle(); button('Play now').emit('click'); await settle();
  assert.equal(h.writes().length, 1); assert.equal(begins, 2); assert.equal(ends, 1); ui.panel.dispose();
});

test('the source UI is embedded and covered by the unchanged browser policy and CI', () => {
  const root = path.join(__dirname, '..');
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  const ci = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.ok(rust.includes('{SOURCE_PLAYER_SCRIPT}'), 'source code is part of the script bundle');
  assert.ok(rust.indexOf('{SCRIPT}') < rust.indexOf('{SOURCE_PLAYER_SCRIPT}'), 'the shared client initializes first');
  assert.ok(rust.includes('{SOURCE_PLAYER_STYLE}'), 'source controls are styled in the bundle');
  assert.match(ci, /run: node --test scripts\/test-remote-source-player.cjs/);
  for (const [file, name] of [['source-player.js', 'SOURCE_PLAYER_SCRIPT'], ['source-player.css', 'SOURCE_PLAYER_STYLE']]) {
    assert.ok(rust.includes(`("${file}", ${name})`));
    const text = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', file), 'utf8').toLowerCase();
    for (const banned of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) assert.ok(!text.includes(banned), `${file}: ${banned}`);
  }
});

test('page-cache transitions consume reviews and remove listeners on disposal', async () => {
  const win = new Element(), doc = new Element(); doc.defaultView = win; doc.hidden = false;
  const ui = panelFixture(async () => []);
  const panel = Player.panel({ call: async name => name === 'zones' ? [zone()] : state(), el: ui.el, document: doc });
  await panel.refresh(); panel.edit('target', 'Den@S2'); panel.edit('input', `spotify:track:${ID}`); await panel.review();
  const p = panel.state.pending; win.emit('pagehide');
  assert.equal(panel.state.pending, null); assert.equal(await panel.confirm(p), false);
  panel.dispose(); assert.equal(win.events.size, 0); assert.equal(doc.events.size, 0); ui.panel.dispose();
});
