// node --test scripts/test-remote-group-volume.cjs; shipped model, DOM and native adapter.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const { once } = require('node:events');
const Group = require('../crates/fsonos-api/src/remote/group-volume.js');
const group = (home = 'S2', members = ['Den', 'Office']) => ({ household: home, coordinator_room: members[0], members });
const snapshot = (volume = 20, zone = group()) => ({ zone, group_volume: volume, volume: 9, mute: true, transport_state: 'paused' });
const settle = async () => { for (let i = 0; i < 50; i++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
function harness(options = {}) {
  const calls = [], views = [];
  let read = async () => snapshot(), write = async () => ({ done: 'Group changed', changed: true, volume: 25 });
  let releases = 0;
  const api = Group.model({ zone: group(), registry: Group.recovery(), changed: v => views.push(v),
    call: (name, args) => { calls.push([name, structuredClone(args)]); return name === 'state' ? read(args) : write(args); },
    acquire: () => () => { releases++; }, ...options });
  api.observe(snapshot(20, options.zone || group()));
  return { api, calls, views, read: fn => { read = fn; }, write: fn => { write = fn; },
    get releases() { return releases; }, get writes() { return calls.filter(([name]) => name === 'volume'); } };
}

test('exact household, coordinator and unique member set identify a group', () => {
  assert.equal(Group.shape(group()), Group.shape({ ...group(), members: ['Office', 'Den'] }));
  for (const bad of [null, {}, group('S2', ['Den']), group('S2', ['Den', 'Den']), { ...group(), coordinator_room: 'Other' }]) assert.throws(() => Group.shape(bad));
  assert.notEqual(Group.shape(group('S1')), Group.shape(group('S2')));
});
test('a room level is never substituted for unavailable or malformed group volume', () => {
  for (const value of [null, undefined, -1, 101, '20', NaN, Infinity, 1.5]) {
    const h = harness(); assert.equal(h.api.observe({ ...snapshot(), group_volume: value }), false);
    assert.equal(h.api.view().ready, false); assert.equal(h.api.preview(30), null);
  }
  const h = harness(); h.api.observe(snapshot(0)); assert.equal(h.api.view().value, 0); assert.equal(h.api.view().ready, true);
});
test('slider edits are immutable local reviews, never writes or automatic ramps', () => {
  const h = harness(); const selected = h.api.preview(80);
  assert.ok(Object.isFrozen(selected)); assert.equal(selected.volume, 80);
  h.api.preview(70); h.api.cancelReview(); assert.equal(h.api.view().review, null);
  assert.equal(h.calls.length, 0);
});
test('invalid and unchanged levels discard prior consent without contacting the API', async () => {
  for (const value of [20, -1, 101, 2.5, NaN, null, '30']) {
    const h = harness(), old = h.api.preview(30); assert.equal(h.api.preview(value), null);
    assert.equal(await h.api.apply(old), false); assert.equal(h.calls.length, 0);
  }
});
test('a confirmed change sends one native group request then reads the observation', async () => {
  const h = harness(); h.read(async () => snapshot(h.writes.length ? 24 : 20));
  assert.equal(await h.api.apply(h.api.preview(30)), true);
  assert.deepEqual(h.calls.map(([n]) => n), ['state', 'volume', 'state']);
  assert.deepEqual(h.writes[0][1], { body: { zone: 'Den@S2', group: true, volume: 30 } });
  assert.equal(h.api.view().value, 24); assert.equal(h.api.view().result.volume, 25);
  assert.equal(h.releases, 1);
});
test('clamped acknowledgements are reported and never ramped toward the requested level', async () => {
  const h = harness(); h.write(async () => ({ done: 'Group capped', changed: true, volume: 22, notes: [{ detail: 'Quiet-hour cap applies.' }] }));
  assert.equal(await h.api.apply(h.api.preview(90)), true);
  assert.match(h.api.view().result.detail, /Quiet-hour cap/); assert.equal(h.writes.length, 1);
  h.api.observe(snapshot(22)); assert.equal(h.api.view().review, null); assert.equal(h.writes.length, 1);
});
test('copied, replaced and canceled reviews cannot approve a new operation', async () => {
  const h = harness(), old = h.api.preview(30), selected = h.api.preview(40);
  for (const review of [old, structuredClone(selected), null]) assert.equal(await h.api.apply(review), false);
  h.api.cancelReview(); assert.equal(await h.api.apply(selected), false); assert.equal(h.calls.length, 0);
});
test('cosmetic observations preserve a review but changed levels and stale events consume it', async () => {
  const h = harness(), selected = h.api.preview(30);
  h.api.observe({ ...snapshot(), track: { title: 'Another song' } }); assert.equal(h.api.view().review, selected);
  h.api.observe(snapshot(21)); assert.equal(await h.api.apply(selected), false);
  const next = h.api.preview(30); h.api.stale(); assert.equal(await h.api.apply(next), false); assert.equal(h.calls.length, 0);
});
test('preflight rejects changed volume, topology and unreadable state with no write', async () => {
  for (const value of [snapshot(21), snapshot(20, group('S1')), snapshot(20, group('S2', ['Den', 'Other'])), null, snapshot(null)]) {
    const h = harness(); h.read(async () => value);
    assert.equal(await h.api.apply(h.api.preview(30)), false); assert.equal(h.writes.length, 0);
    assert.equal(h.api.view().result.status, 'not_sent'); assert.equal(h.releases, 1);
  }
});
test('an event arriving during preflight invalidates it even if the old read returns later', async () => {
  const h = harness(), pending = deferred(); h.read(() => pending.promise);
  const action = h.api.apply(h.api.preview(30)); h.api.stale(); pending.resolve(snapshot());
  assert.equal(await action, false); assert.equal(h.writes.length, 0); assert.equal(h.releases, 1);
});
test('failed or denied acquisition never issues a read or a volume command', async () => {
  for (const acquire of [() => null, () => { throw new Error('busy'); }]) {
    const h = harness({ acquire }); assert.equal(await h.api.apply(h.api.preview(30)), false);
    assert.equal(h.calls.length, 0); assert.equal(h.api.view().result.status, 'not_sent');
  }
});
test('double confirmation and edits cannot overlap an outstanding command', async () => {
  const h = harness(), pending = deferred(); h.write(() => pending.promise);
  const selected = h.api.preview(30), action = h.api.apply(selected); await settle();
  assert.equal(await h.api.apply(selected), false); assert.equal(h.api.preview(40), null);
  assert.equal(await h.api.refresh(), false); assert.equal(h.writes.length, 1);
  pending.resolve({ done: 'Group changed', changed: true, volume: 30 }); await action;
  assert.equal(h.releases, 1); assert.equal(await h.api.apply(selected), false);
});
test('lost and malformed write replies remain uncertain through passive observations', async () => {
  for (const reply of [new Error('lost'), null, {}, { done: 'ok', volume: 25 }, { done: 'ok', changed: true, volume: 101 },
    { done: 'ok', changed: true, volume: 25, notes: [null] }]) {
    const h = harness(); h.write(async () => { if (reply instanceof Error) throw reply; return reply; });
    assert.equal(await h.api.apply(h.api.preview(30)), false); h.api.observe(snapshot(30));
    assert.equal(h.api.view().uncertain, true); assert.equal(h.api.preview(40), null);
    assert.equal(h.writes.length, 1); assert.equal(h.calls.length, 2);
    assert.equal(await h.api.refresh(), true); assert.equal(h.api.view().uncertain, false);
    assert.equal(h.api.view().ready, true); assert.equal(h.writes.length, 1);
  }
});
test('an unsuccessful explicit read cannot clear an uncertain result', async () => {
  const h = harness(); h.write(async () => { throw new Error('lost'); }); await h.api.apply(h.api.preview(30));
  h.read(async () => snapshot(null)); assert.equal(await h.api.refresh(), false);
  assert.equal(h.api.view().uncertain, true); assert.equal(h.api.preview(40), null);
});
test('accepted but unreadable readback does not become a failed or repeated command', async () => {
  const h = harness(); h.read(async () => { if (h.writes.length) throw new Error('offline'); return snapshot(); });
  assert.equal(await h.api.apply(h.api.preview(30)), true); assert.equal(h.api.view().result.status, 'accepted');
  assert.equal(h.api.view().fresh, false); assert.equal(h.writes.length, 1);
});
test('an explicit read cannot overwrite a later shared observation', async () => {
  const h = harness(), pending = deferred(); h.read(() => pending.promise);
  const read = h.api.refresh(); h.api.observe(snapshot(70)); pending.resolve(snapshot(20));
  assert.equal(await read, false); assert.equal(h.api.view().value, 70);
});
test('hide, dispose and hide-show invalidate unsent work and release its reservation', async () => {
  for (const stop of [api => api.suspend(), api => api.dispose(), api => { api.suspend(); api.resume(); }]) {
    const h = harness(), pending = deferred(); h.read(() => pending.promise);
    const action = h.api.apply(h.api.preview(30)); stop(h.api); pending.resolve(snapshot());
    assert.equal(await action, false); assert.equal(h.writes.length, 0); assert.equal(h.releases, 1);
  }
});
test('a hidden late failure preserves uncertainty without publishing to a hidden view', async () => {
  const h = harness(), pending = deferred(); h.write(() => pending.promise);
  const action = h.api.apply(h.api.preview(30)); await settle(); h.api.suspend(); const count = h.views.length;
  pending.reject(new Error('lost')); await action; assert.equal(h.views.length, count);
  h.api.resume(); h.api.observe(snapshot(25)); assert.equal(h.api.view().uncertain, true);
  assert.match(h.api.view().result.detail, /not repeated/); assert.equal(h.writes.length, 1);
});
test('uncertainty follows coordinator elections and each split group recovers independently', async () => {
  const registry = Group.recovery(), original = group('S2', ['A', 'B', 'C', 'D']);
  const h = harness({ zone: original, registry }); h.read(async () => snapshot(20, original));
  h.write(async () => { throw new Error('lost'); }); await h.api.apply(h.api.preview(30)); h.api.dispose();
  const a = harness({ zone: group('S2', ['B', 'A']), registry }), b = harness({ zone: group('S2', ['D', 'C']), registry });
  assert.equal(a.api.view().uncertain, true); assert.equal(b.api.view().uncertain, true);
  a.read(async () => snapshot(25, group('S2', ['B', 'A']))); await a.api.refresh();
  assert.equal(a.api.view().ready, true); assert.equal(b.api.view().uncertain, true);
  assert.equal(harness({ zone: group('S1', ['B', 'A']), registry }).api.view().ready, true);
});
test('a merged group retains both outstanding operations until both settle', () => {
  const r = Group.recovery(), first = r.start(['a', 'b']), second = r.start(['c', 'd']);
  r.finish(first, true); r.clear(['a', 'b']); assert.deepEqual(r.status(['a', 'b', 'c', 'd']), { pending: true, uncertain: false });
  r.finish(second, true); assert.deepEqual(r.status(['a', 'b', 'c', 'd']), { pending: false, uncertain: true });
  r.clear(['a', 'b', 'c', 'd']); assert.deepEqual(r.status(['a', 'b', 'c', 'd']), { pending: false, uncertain: false });
});
test('exceptions in a view cannot turn accepted commands into failures', async () => {
  const h = harness({ changed() { throw new Error('view'); } });
  assert.equal(await h.api.apply(h.api.preview(30)), true); assert.equal(h.writes.length, 1);
});

class Element {
  constructor(tag = '') { this.tag = tag; this.children = []; this.events = new Map(); this.disabled = false; this.value = ''; this.textContent = ''; this.attrs = {}; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  setAttribute(k, v) { this.attrs[k] = String(v); if (k === 'disabled') this.disabled = true; if (k === 'value') this.value = String(v); if (k === 'hidden') this.hidden = true; }
  addEventListener(k, f) { if (!this.events.has(k)) this.events.set(k, new Set()); this.events.get(k).add(f); }
  removeEventListener(k, f) { this.events.get(k)?.delete(f); }
  emit(k, event = {}) { for (const f of this.events.get(k) || []) f(event); }
}
function el(tag, attrs = {}, ...children) {
  const node = new Element(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === false || v == null) continue;
    if (k === 'text') node.textContent = v;
    else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v === true ? '' : v);
  }
  node.append(...children); return node;
}
const nodes = n => [n, ...n.children.flatMap(v => typeof v === 'object' ? nodes(v) : [])];
const button = (n, label) => nodes(n).find(v => v.tag === 'button' && v.textContent === label);
function attached(zone = group()) {
  const calls = [], registry = Group.recovery(), flights = new Map(), doc = new Element(), win = new Element(); doc.hidden = false;
  let read = async () => snapshot(20, zone), write = async () => ({ done: 'Group changed', changed: true, volume: 30 });
  let begin = 0, end = 0, cards = new Map();
  const entry = room => ({ room, input: new Element('input'), version: 0, timer: null, pendingVolume: null });
  const parts = { zone, writing: false, disposed: false, sliders: new Map(zone.members.map(room => [room, entry(`${room}@${zone.household}`)])) };
  cards.set('group', parts);
  const env = { get cards() { return cards; }, volumeFlights: flights, registry, document: doc, window: win, el,
    ownsCard: p => !p.disposed && [...cards.values()].includes(p), transportBusy: p => p.writing,
    beginAction(p) { begin++; p.writing = true; return true; }, endAction(p) { end++; p.writing = false; },
    call: (n, a) => { calls.push([n, a]); return n === 'state' ? read(a) : write(a); } };
  const panel = Group.attach(parts, env); panel.observe(snapshot(20, zone));
  return { panel, parts, env, calls, flights, doc, win, registry, entry, get begin() { return begin; }, get end() { return end; },
    read(fn) { read = fn; }, write(fn) { write = fn; }, replace(next) { parts.disposed = true; cards = new Map(next.map((p, i) => [i, p])); } };
}
test('the mounted slider requires confirmation and retired buttons cannot apply a replacement review', async () => {
  const h = attached(), range = nodes(h.panel.node).find(n => n.tag === 'input');
  range.value = '30'; range.emit('input'); const old = button(h.panel.node, 'Set group to 30'); assert.ok(old); assert.equal(h.calls.length, 0);
  range.value = '40'; range.emit('input'); old.emit('click'); await settle(); assert.equal(h.calls.length, 0);
  button(h.panel.node, 'Set group to 40').emit('click'); await settle();
  assert.equal(h.calls.filter(([n]) => n === 'volume').length, 1); assert.equal(h.begin, 1); assert.equal(h.end, 1); h.panel.dispose();
});
test('the native adapter reserves every room slider and fences observations across completion', async () => {
  const h = attached(), hold = deferred(); h.write(() => hold.promise);
  const action = h.panel.apply(h.panel.preview(30)); await settle();
  assert.deepEqual([...h.flights.keys()], ['Den@S2', 'Office@S2']);
  assert.ok([...h.parts.sliders.values()].every(e => e.input.disabled && e.version === 1));
  hold.resolve({ done: 'Group changed', changed: true, volume: 25 }); await action;
  assert.equal(h.flights.size, 0); assert.equal(h.end, 1);
  assert.ok([...h.parts.sliders.values()].every(e => e.version === 2)); h.panel.dispose();
});
test('pending individual volume writes and unsent drafts prevent group dispatch', async () => {
  for (const busy of [h => h.flights.set('Den@S2', {}), h => h.parts.sliders.get('Den').timer = 10,
    h => h.parts.sliders.get('Office').pendingVolume = 30, h => h.parts.writing = true]) {
    const h = attached(), selected = h.panel.preview(30); busy(h);
    assert.equal(await h.panel.apply(selected), false); assert.equal(h.calls.length, 0); assert.equal(h.begin, 0); h.panel.dispose();
  }
});
test('replacement cards inherit pending group writes and release only their original reservations', async () => {
  const h = attached(), hold = deferred(); h.write(() => hold.promise);
  const action = h.panel.apply(h.panel.preview(30)); await settle();
  const replacement = { zone: group('S2', ['Office', 'Den']), writing: false, sliders: new Map([
    ['Office', h.entry('Office@S2')], ['Den', h.entry('Den@S2')]]) };
  h.panel.dispose(); h.replace([replacement]); const next = Group.attach(replacement, h.env);
  assert.equal(next.observe(snapshot(20, replacement.zone)), false); assert.equal(next.preview(40), null);
  hold.reject(new Error('lost')); await action;
  assert.equal(h.flights.size, 0); assert.equal(h.end, 1); assert.equal(next.view().uncertain, true);
  assert.ok([...replacement.sliders.values()].every(e => e.version === 1)); next.dispose();
});
test('hiding the mounted panel consumes confirmation and disposal removes its listeners', async () => {
  const h = attached(); h.panel.preview(30); const old = button(h.panel.node, 'Set group to 30');
  h.doc.hidden = true; h.doc.emit('visibilitychange'); old.emit('click'); await settle(); assert.equal(h.calls.length, 0);
  h.doc.hidden = false; h.doc.emit('visibilitychange'); assert.equal(h.panel.view().ready, false);
  h.panel.dispose(); assert.equal(h.doc.events.get('visibilitychange').size, 0); assert.equal(h.win.events.get('pagehide').size, 0);
});
test('the production registration stays lazy and omits group controls on single rooms', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/group-volume.js'), 'utf8');
  let factory, calls = 0;
  vm.runInNewContext(source, { registerCardPanel: fn => { factory = fn; }, el, house: { cards: new Map() }, volumeFlights: new Map(),
    ownsCard: () => true, transportBusy: () => false, beginAction: () => true, endAction() {}, call: () => { calls++; }, document: new Element(), window: new Element() });
  assert.equal(calls, 0); assert.equal(factory({ zone: group('S2', ['Den']) }).node.hidden, true);
});

for (const lost of [false, true]) test(`real loopback HTTP carries one qualified group request (${lost ? 'lost response' : 'accepted'})`, async t => {
  const requests = [], zone = group('S1', ['A & B/?', 'Other']); let volume = 20;
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null; requests.push([req.method, req.url, body]);
    if (req.method === 'POST') { volume = Math.min(body.volume, 24); if (lost) { req.socket.destroy(); return; } }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(req.method === 'POST' ? { done: 'Group capped', changed: true, volume } : snapshot(volume, zone)));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening'); t.after(() => { server.closeAllConnections(); server.close(); });
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = harness({ zone, call: async (name, args) => {
    const url = name === 'state' ? `/zones/${encodeURIComponent(args.params.room)}/state` : '/volume';
    const answer = await fetch(base + url, { method: name === 'state' ? 'GET' : 'POST', headers: { 'Content-Type': 'application/json' },
      ...(name === 'state' ? {} : { body: JSON.stringify(args.body) }) });
    return answer.json();
  } });
  assert.equal(await h.api.apply(h.api.preview(80)), !lost);
  if (lost) { h.api.observe(snapshot(volume, zone)); assert.equal(h.api.view().uncertain, true); await h.api.refresh(); }
  assert.equal(h.api.view().value, 24);
  assert.deepEqual(requests.filter(([method]) => method === 'POST'), [['POST', '/volume', { zone: 'A & B/?@S1', group: true, volume: 80 }]]);
  assert.ok(requests.filter(([method]) => method === 'GET').every(([, url]) => url === '/zones/A%20%26%20B%2F%3F%40S1/state'));
});

test('the daemon embeds group controls after the shared client and scans both new assets', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const rust = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  assert.ok(rust.includes('{FADES_SCRIPT}\\n{GROUP_VOLUME_SCRIPT}'));
  assert.ok(rust.includes('{FADES_STYLE}\\n{GROUP_VOLUME_STYLE}'));
  for (const [file, constant] of [['group-volume.js', 'GROUP_VOLUME_SCRIPT'], ['group-volume.css', 'GROUP_VOLUME_STYLE']]) {
    assert.ok(rust.includes(`include_str!("../remote/${file}")`));
    assert.ok(rust.includes(`("${file}", ${constant})`));
    const text = fs.readFileSync(path.join(root, 'remote', file), 'utf8').toLowerCase();
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.equal(text.includes(secret), false, `${file}: ${secret}`);
    }
  }
});
