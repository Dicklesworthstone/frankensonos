// node --test scripts/test-remote-room-sets.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const fs = require('node:fs');
const path = require('node:path');
const Sets = require('../crates/fsonos-api/src/remote/room-sets.js');
const room = (household, name, player, coordinator = player) => ({ household, room: name, player, coordinator });
const den = room('S1', 'Den', 'sim-s1-den');
const k1 = room('S1', 'Kitchen', 'sim-s1-kit', den.player);
const k2 = room('S2', 'Kitchen', 'sim-s2-kit');
const group = (leader, members) => ({ household: leader.household, coordinator: leader.player, coordinator_room: leader.room, members });
const sample = (target = 'downstairs') => ({ target, matched: 'alias', rooms: [k1, k2], groups: [group(den, [den, k1]), group(k2, [k2])] });
const clone = v => structuredClone(v);
const stateOf = g => ({ zone: { household: g.household, coordinator_room: g.coordinator_room, members: g.members.map(r => r.room) },
  transport_state: 'paused', track: { uri: 'test:source', title: '<em>A recording</em>', queue_position: 2, position_secs: 10 } });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
function harness(options = {}) {
  const h = { value: sample(), calls: [], custom: null, choices: [{ name: 'Kitchen', household: 'S1', zone: 'Den', aliases: ['downstairs'] }], executions: 0 };
  h.api = Sets.model({ call: async (name, args) => {
    h.calls.push([name, args]);
    if (h.custom) { const response = h.custom(name, args); if (response !== undefined) return response; }
    if (name === 'setRooms') return clone(h.choices);
    if (name === 'resolveRooms') return clone({ ...h.value, target: args.query.target });
    if (name === 'state') {
      const g = h.value.groups.find(g => `${g.coordinator_room}@${g.household}` === args.params.room);
      return stateOf(g);
    }
    return { done: 'Collective request completed', changed: true, notes: [] };
  }, execute: async (groups, work) => { h.executions++; h.locked = groups; return work(); }, ...options });
  h.writeCalls = () => h.calls.filter(([name]) => ['setPause', 'setResume', 'setMute', 'setVolume'].includes(name));
  h.open = async (action = 'pause', amount = '5') => { h.api.open(); h.api.edit('input', 'downstairs'); h.api.edit('action', action); h.api.edit('amount', amount); };
  h.review = async (action, amount) => { await h.open(action, amount); assert.equal(await h.api.review(), true); return h.api.state.preview; };
  return h;
}

test('resolution distinguishes selected rooms from full transport groups and ignores ordering', () => {
  const value = sample(), a = Sets.resolutionOf(value, 'downstairs');
  assert.equal(a.rooms.length, 2); assert.equal(a.groups.flatMap(g => g.members).length, 3);
  assert.ok(Object.isFrozen(a) && Object.isFrozen(a.groups[0].members[0]));
  value.rooms.reverse(); value.groups.reverse(); value.groups[1].members.reverse();
  assert.equal(Sets.resolutionOf(value, 'downstairs').signature, a.signature);
  value.groups[0].coordinator_room = 'Changed'; assert.throws(() => Sets.resolutionOf(value, 'downstairs'));
});

test('incomplete, duplicate, cross-household and excess resolution data is refused', () => {
  const changes = [v => { v.target = 'other'; }, v => { v.matched = 'guess'; }, v => { v.rooms = []; },
    v => { v.groups = []; }, v => v.rooms.push(v.rooms[0]), v => v.groups.push(v.groups[0]),
    v => v.groups[0].members.push(v.groups[0].members[0]), v => { v.rooms[0] = { ...v.rooms[0], player: 'different' }; },
    v => { v.groups[0].members[0].household = 'S2'; }, v => { v.rooms[0] = { ...v.rooms[0], coordinator: 'other' }; },
    v => { v.groups[0].members = [den]; }, v => { v.rooms[0].room = ''; },
    v => { v.rooms = Array(513).fill(k1); }, v => { v.groups[0].coordinator_room = 'absent'; }];
  for (const change of changes) { const v = clone(sample()); change(v); assert.throws(() => Sets.resolutionOf(v, 'downstairs')); }
  for (const v of [null, {}, []]) assert.throws(() => Sets.resolutionOf(v, 'downstairs'));
});

test('suggestions are not a second alias resolver and no destination is automatically selected', async () => {
  const h = harness(); await h.api.refresh();
  assert.equal(h.api.state.input, ''); assert.equal(h.writeCalls().length, 0);
  assert.ok(h.api.state.suggestions.some(x => x.query === 'all@S1'));
  h.api.edit('input', 'downstairs'); await h.api.review();
  assert.equal(h.api.state.preview.view.rooms.length, 2, 'actual resolver includes the unlisted S2 alias member');
  assert.equal(h.calls.filter(([name]) => name === 'setRooms').length, 1);
});

test('suggestions reject duplicates and malformed aliases; empty inventory still permits explicit input', () => {
  const one = { name: 'Kitchen', household: 'S1', zone: 'Den' };
  assert.equal(Sets.suggestionsOf([]).length, 2);
  for (const v of [null, [one, one], [{ ...one, aliases: [null] }], [{ ...one, zone: '' }]]) assert.throws(() => Sets.suggestionsOf(v));
});

for (const [action, amount, route, fields] of [
  ['pause', '5', 'setPause', {}], ['resume', '5', 'setResume', {}], ['mute', '5', 'setMute', { mute: true }],
  ['unmute', '5', 'setMute', { mute: false }], ['lower', '7', 'setVolume', { group: false, delta: -7 }],
  ['raise', '3', 'setVolume', { group: false, delta: 3 }], ['volume', '0', 'setVolume', { group: false, volume: 0 }],
]) test(`${action} uses exactly one existing set endpoint after review`, async () => {
  const h = harness(), preview = await h.review(action, amount);
  assert.equal(h.writeCalls().length, 0); assert.ok(Object.isFrozen(preview.request.body));
  assert.equal(await h.api.confirm(preview), true);
  assert.deepEqual(h.writeCalls(), [[route, { body: { zone: 'downstairs', ...fields } }]]);
  assert.equal(h.locked.length, 2); assert.equal(h.api.state.result.status, 'accepted');
  assert.equal(h.calls.filter(([name]) => name === 'state').length, action === 'resume' ? 4 : 0);
});

test('invalid amounts and target names never reach the daemon', async () => {
  for (const [action, amount] of [['raise', '0'], ['lower', '-5'], ['volume', '101'], ['volume', '2.5'], ['lower', '5x']]) {
    const h = harness(); await h.open(action, amount); assert.equal(await h.api.review(), false); assert.equal(h.calls.length, 0);
  }
  for (const input of ['', 'x'.repeat(129), 'bad\nname']) {
    const h = harness(); await h.open(); h.api.edit('input', input); assert.equal(await h.api.review(), false); assert.equal(h.calls.length, 0);
  }
});

test('a denied or missing resolver never falls back to guessed room membership', async () => {
  for (const status of [403, 404, 503]) {
    const h = harness(); await h.open(); h.custom = () => { throw Object.assign(new Error('Cannot resolve'), { status, hint: 'Check permission.' }); };
    assert.equal(await h.api.review(), false); assert.equal(h.writeCalls().length, 0);
    assert.match(h.api.state.message, /Check permission/); assert.equal(h.calls.length, 1);
  }
});

test('membership, IDs and match-kind changes consume the review without sending a command', async () => {
  for (const change of [h => { h.value.rooms = [k1]; h.value.groups = [group(den, [den, k1])]; },
    h => { h.value.matched = 'room'; }, h => { h.value.rooms[1].player = 'replaced'; h.value.groups[1].members[0].player = 'replaced'; },
    h => { h.value.groups[0].members.push(room('S1', 'Office', 'sim-office', den.player)); }]) {
    const h = harness(); h.value = clone(h.value); const p = await h.review(); change(h);
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writeCalls().length, 0); assert.equal(h.api.state.result.status, 'not_sent');
  }
});

test('a disappearing or changing source blocks a collective resume but position progression does not', async () => {
  for (const edit of [s => { s.track.uri = 'test:other'; }, s => { s.track.queue_position++; },
    s => { s.transport_state = 'playing'; }, s => { s.track = null; }, s => { s.zone.household = 'wrong'; }]) {
    const h = harness(), p = await h.review('resume'); h.custom = (name, args) => {
      if (name !== 'state') return undefined;
      const s = stateOf(h.value.groups.find(g => `${g.coordinator_room}@${g.household}` === args.params.room)); edit(s); return s;
    };
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writeCalls().length, 0);
  }
  const h = harness(), p = await h.review('resume');
  h.custom = (name, args) => { if (name === 'state') { const s = stateOf(h.value.groups.find(g => `${g.coordinator_room}@${g.household}` === args.params.room)); s.track.position_secs = 90; return s; } };
  assert.equal(await h.api.confirm(p), true);
});

test('resume preflight has at most four group state reads in flight', async () => {
  const h = harness();
  h.value.rooms = Array.from({ length: 9 }, (_, i) => room('S1', `Room ${i}`, `sim-${i}`));
  h.value.groups = h.value.rooms.map(r => group(r, [r]));
  let active = 0, maximum = 0;
  h.custom = async (name, args) => { if (name !== 'state') return name === 'resolveRooms' ? clone({ ...h.value, target: args.query.target }) : undefined;
    active++; maximum = Math.max(maximum, active); await new Promise(r => setImmediate(r)); active--;
    return stateOf(h.value.groups.find(g => `${g.coordinator_room}@${g.household}` === args.params.room));
  };
  await h.open('resume'); assert.equal(await h.api.review(), true); assert.equal(maximum, 4); assert.equal(active, 0);
});

test('partial outcomes retain every skipped-room and cap note', async () => {
  const h = harness(), p = await h.review('lower');
  h.custom = name => name === 'setVolume' ? { done: 'Changed reachable rooms', changed: true,
    notes: [{ code: 'SKIPPED', detail: 'One room is offline.' }, { code: 'CLAMPED', detail: 'The quiet-hours limit applied.' }] } : undefined;
  assert.equal(await h.api.confirm(p), true); assert.equal(h.api.state.result.partial, true);
  assert.match(h.api.state.result.detail, /offline/); assert.match(h.api.state.result.detail, /quiet-hours/);
  assert.equal(h.api.state.result.notes.length, 2);
});

test('double clicks and a duplicated action-lane callback cannot repeat a write', async () => {
  const h = harness({ execute: async (_, work) => Promise.all([work(), work()]) });
  const p = await h.review(); await Promise.all([h.api.confirm(p), h.api.confirm(p)]);
  assert.equal(h.writeCalls().length, 1);
});

test('editing, copied reviews, cancellation and closing make old confirmations unusable', async () => {
  for (const act of [h => h.api.edit('input', 'all@S1'), h => h.api.edit('action', 'mute'), h => h.api.edit('amount', '8'),
    h => h.api.cancel(), h => h.api.close(), h => h.api.dispose()]) {
    const h = harness(), p = await h.review(); act(h); assert.equal(await h.api.confirm(p), false); assert.equal(h.writeCalls().length, 0);
  }
  const h = harness(), p = await h.review(); assert.equal(await h.api.confirm(clone(p)), false);
});

test('an abort-ignoring review retains its read slot and cannot repopulate a changed draft', async () => {
  const h = harness(), pending = deferred(); await h.open(); h.custom = name => name === 'resolveRooms' ? pending.promise : undefined;
  const read = h.api.review(); await settle(); h.api.edit('input', 'all@S1'); h.api.close(); h.api.open();
  assert.equal(await h.api.review(), false); assert.equal(await h.api.refresh(), false);
  assert.equal(h.calls.length, 1); assert.equal(h.calls[0][1].signal.aborted, true);
  pending.resolve(sample()); assert.equal(await read, false); assert.equal(h.api.state.preview, null); assert.equal(h.api.state.reading, false);
});

test('closing during preflight aborts the operation before dispatch', async () => {
  const h = harness(), p = await h.review(), wait = deferred(); h.custom = name => name === 'resolveRooms' ? wait.promise : undefined;
  const action = h.api.confirm(p); await settle(); h.api.close(); wait.resolve(sample());
  assert.equal(await action, false); assert.equal(h.writeCalls().length, 0); assert.equal(h.api.state.writing, false);
});

test('closing and reopening a sent request keeps the write slot and its eventual receipt', async () => {
  const h = harness(), p = await h.review(), wait = deferred(); h.custom = name => name === 'setPause' ? wait.promise : undefined;
  const action = h.api.confirm(p); await settle(); h.api.close(); h.api.open();
  assert.equal(h.api.edit('action', 'resume'), false); assert.equal(await h.api.review(), false);
  assert.equal(h.api.state.writing, true); wait.resolve({ done: 'Paused', changed: true });
  assert.equal(await action, true); assert.equal(h.writeCalls().length, 1); assert.equal(h.api.state.result.detail, 'Paused');
});

test('lost or malformed responses stay uncertain, never becoming an automatic retry', async () => {
  for (const value of [new Error('reply lost'), null, {}, { done: 'x', changed: 'yes' }, { done: 'x', changed: true, notes: [null] }]) {
    const h = harness(), p = await h.review(); h.custom = name => { if (name !== 'setPause') return undefined; if (value instanceof Error) throw value; return value; };
    assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain');
    h.api.close(); await h.api.refresh(); assert.equal(await h.api.confirm(p), false);
    assert.equal(h.writeCalls().length, 1); assert.match(h.api.state.result.detail, /not repeated/);
  }
});

test('a presentation failure after success does not turn the accepted result into uncertainty', async () => {
  const h = harness({ changed() { throw new Error('render'); }, execute: async (_, work) => { await work(); throw new Error('presentation cleanup'); } });
  const p = await h.review(); assert.equal(await h.api.confirm(p), true); assert.equal(h.api.state.result.status, 'accepted');
});

test('group coordination acquires all existing lanes or releases its partial acquisition', async () => {
  const view = Sets.resolutionOf(sample(), 'downstairs'), cards = view.groups.map(g => ({ zone: g.shape }));
  const acquired = [], released = []; let work = 0;
  const run = Sets.coordinated({ cards: () => cards, refresh: async () => {}, shape: z => z,
    begin: p => { acquired.push(p); return acquired.length !== 2; }, end: p => released.push(p) });
  await assert.rejects(run(view.groups, async () => { work++; }));
  assert.equal(work, 0); assert.deepEqual(released, [cards[0]]);
});

test('group coordination refreshes missing cards once and releases every lane after an error', async () => {
  const view = Sets.resolutionOf(sample(), 'downstairs'); let cards = [], refreshed = 0, releases = 0;
  const run = Sets.coordinated({ cards: () => cards, refresh: async () => { refreshed++; cards = view.groups.map(g => ({ zone: g.shape })); },
    shape: z => z, begin: () => true, end: () => { releases++; throw new Error('view'); } });
  await assert.rejects(run(view.groups, async () => { throw new Error('operation'); }), /operation/);
  assert.equal(refreshed, 1); assert.equal(releases, 2);
});

test('real localhost HTTP preserves a punctuation-rich alias and makes one collective POST', async t => {
  const target = 'A + B & upstairs/?#', calls = [];
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost'), chunks = [];
    for await (const c of req) chunks.push(c);
    calls.push([req.method, url.pathname, url.searchParams.get('target'), chunks.length ? JSON.parse(Buffer.concat(chunks)) : null]);
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(req.method === 'POST' ? { done: 'Adjusted the selected rooms', changed: true }
      : sample(url.searchParams.get('target'))));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const root = `http://127.0.0.1:${server.address().port}`;
  const api = Sets.model({ call: async (name, args = {}) => {
    const [method, route] = Sets.routes[name], query = args.query ? '?' + new URLSearchParams(args.query) : '';
    const response = await fetch(root + route + query, { method, headers: { 'content-type': 'application/json' },
      ...(args.body ? { body: JSON.stringify(args.body) } : {}) });
    return response.json();
  } });
  api.open(); api.edit('input', target); api.edit('action', 'lower'); api.edit('amount', '6');
  assert.equal(await api.review(), true); assert.equal(await api.confirm(api.state.preview), true);
  assert.deepEqual(calls.filter(([method]) => method === 'POST'), [['POST', '/volume', null, { zone: target, group: false, delta: -6 }]]);
  assert.equal(calls.length, 3); assert.ok(calls.slice(0, 2).every(call => call[2] === target));
});

function sliders(view) {
  return view.groups.map(g => ({ zone: g.shape, sliders: new Map(g.members.map(r => [r.room, {
    room: Sets.roomName(r), input: { disabled: false }, mute: { disabled: false }, timer: null,
    pendingVolume: null, volumeWriting: false, mutedPending: false, version: 0,
  }])) }));
}

test('collective operations reserve the existing slider lanes without consuming queued drafts', () => {
  const view = Sets.resolutionOf(sample(), 'downstairs');
  for (const pending of ['timer', 'pendingVolume', 'volumeWriting', 'mutedPending', 'flight']) {
    const cards = sliders(view), flights = new Map();
    const entries = cards.flatMap(c => [...c.sliders.values()]), entry = entries[0];
    if (pending === 'flight') flights.set(entry.room, entry); else entry[pending] = pending === 'pendingVolume' ? 17 : true;
    const lanes = Sets.levelLanes({ cards: () => cards, flights });
    assert.equal(lanes.reserve(view.groups), null);
    assert.equal(entry[pending], pending === 'flight' ? undefined : pending === 'pendingVolume' ? 17 : true);
    assert.equal(flights.size, pending === 'flight' ? 1 : 0);
    assert.ok(entries.every(e => !e.input.disabled && !e.mute.disabled));
  }
});

test('a replacement card inherits volume and mute exclusion until the original operation exits', () => {
  const view = Sets.resolutionOf(sample(), 'downstairs'); let cards = sliders(view);
  const flights = new Map(), lanes = Sets.levelLanes({ cards: () => cards, flights });
  const release = lanes.reserve(view.groups);
  assert.equal(typeof release, 'function'); assert.equal(flights.size, 3);
  for (const c of cards) for (const e of c.sliders.values()) {
    assert.ok(e.input.disabled && e.mute.disabled && e.mutedPending); assert.equal(e.version, 1);
  }
  cards = sliders(view);
  for (const c of cards) lanes.observe(c);
  assert.equal(lanes.reserve(view.groups), null);
  for (const c of cards) for (const e of c.sliders.values()) assert.ok(e.mutedPending && e.input.disabled);
  release(); release();
  assert.equal(flights.size, 0);
  for (const c of cards) for (const e of c.sliders.values()) {
    assert.ok(e.input.disabled, 'wait for the fresh read instead of making old volume current');
    assert.ok(!e.mute.disabled && !e.mutedPending); assert.equal(e.version, 2);
  }
});

test('partial group-lock failure returns every reserved level lane', async () => {
  const view = Sets.resolutionOf(sample(), 'downstairs'), cards = sliders(view), flights = new Map();
  const levels = Sets.levelLanes({ cards: () => cards, flights });
  let started = 0, ended = 0;
  const execute = Sets.coordinated({ cards: () => cards, refresh: async () => {}, shape: z => z,
    begin: () => ++started < 2, end: () => { ended++; assert.equal(flights.size, 0); }, levels });
  await assert.rejects(execute(view.groups, () => assert.fail('must not run')));
  assert.equal(started, 2); assert.equal(ended, 1); assert.equal(flights.size, 0);
  assert.ok(cards.flatMap(c => [...c.sliders.values()]).every(e => !e.mutedPending));
});

test('the studio embeds the controller before its panel and classifies it before generic grouping', () => {
  const root = path.join(__dirname, '..');
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote/studio.rs'), 'utf8');
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  const Studio = require('../crates/fsonos-api/src/remote/studio.js');
  assert.ok(rust.indexOf('{ROOM_SETS_SCRIPT}') < rust.indexOf('{ROOM_SETS_PANEL}'));
  assert.ok(rust.indexOf('{ROOM_SETS_PANEL}') < rust.indexOf('{SCRIPT}'));
  assert.ok(rust.includes('{ROOM_SETS_STYLE}'));
  assert.match(workflow, /run: node --test scripts\/test-remote-groups.cjs scripts\/test-remote-transfers.cjs scripts\/test-remote-room-sets.cjs/);
  const tool = Studio.identify({ id: 'room-set-controls', getAttribute: () => 'Room sets and household controls', querySelector: () => null });
  assert.equal(tool.id, 'room-sets'); assert.equal(tool.workspace, 'house');
  assert.equal(Studio.search('alias volume', [tool]).length, 1);
  for (const file of ['room-sets.js', 'room-sets-panel.js', 'room-sets.css']) {
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', file), 'utf8').toLowerCase();
    for (const bad of ['authorization', 'password', 'cookie', 'http://', 'https://', 'innerhtml', 'eval(']) assert.ok(!source.includes(bad), `${file}: ${bad}`);
  }
});
