'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const zone = { household: 'S2', coordinator_room: 'Study / room', members: ['Study / room', 'Kitchen'] };
const tick = () => new Promise((resolve) => setImmediate(resolve));
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
function row(position) {
  return { position, title: `Track ${position}`, creator: 'An artist', album: 'An album', uri: 'same:duplicate' };
}
function page(offset = 0, total = 120, revision = 'revision:one', z = zone) {
  return { zone: z.coordinator_room, total, revision, playing: total ? 2 : null,
    items: Array.from({ length: Math.min(40, Math.max(0, total - offset)) }, (_, index) => row(offset + index + 1)) };
}
function result({ query = 'Track', total = 120, revision = 'revision:one', offset = 0, end = total, positions = [42, 44] } = {}) {
  return { zone: zone.coordinator_room, query, total, revision, offset, examined: end - offset,
    next_offset: end < total ? end : null, complete: end === total, items: positions.map(row) };
}
async function fixture(total = 120) {
  const calls = [], item = { zone: structuredClone(zone), fresh: true };
  let answer = (url) => {
    const offset = Number(new URL(url, 'https://fixture').searchParams.get('offset'));
    return url.includes('/queue/search?') ? result({ total }) : page(offset, total);
  };
  let changed = () => {};
  const model = Q.model({ call: async (url, body, options) => {
    calls.push({ url, body, options }); return answer(url, body, options);
  }, changed: (state) => changed(state) });
  model.observe(item); model.open(); await tick();
  return { model, calls, item, answer(fn) { answer = fn; }, changed(fn) { changed = fn; } };
}

test('search uses one bounded native GET pinned to the selected household and queue revision', async () => {
  const f = await fixture(), m = f.model;
  assert.equal(await m.search('  Track  '), true);
  assert.equal(f.calls.length, 2);
  assert.equal(f.calls[1].url, '/zones/Study%20%2F%20room%40S2/queue/search?q=Track&offset=0&limit=50&expected_revision=revision%3Aone');
  assert.equal(f.calls[1].body, undefined); assert.ok(f.calls[1].options.signal);
  assert.deepEqual(m.state.search.items.map((r) => r.position), [42, 44]);
  assert.equal(m.state.search.items[0].uri, m.state.search.items[1].uri);
  assert.equal(m.state.query, 'Track'); assert.equal(m.state.loading, false);
  assert.throws(() => { m.state.search.items[0].position = 1; });
  assert.throws(() => m.state.search.items.push(row(9)));
});

test('continuation is explicit, preserves the revision and replaces its bounded section', async () => {
  const f = await fixture(12000), m = f.model;
  f.answer((url) => {
    const u = new URL(url, 'https://fixture'), offset = Number(u.searchParams.get('offset'));
    return result({ total: 12000, offset, end: offset ? 12000 : 9900, positions: offset ? [11000] : [42] });
  });
  assert.equal(await m.search('Track'), true);
  const first = m.state.search; assert.equal(first.complete, false); assert.equal(f.calls.length, 2);
  assert.equal(first.next_offset, 9900);
  assert.equal(await m.search('Track', { ...first }), false);
  assert.equal(await m.search('Changed', first), false);
  assert.equal(f.calls.length, 2);
  assert.equal(await m.search('Track', first), true);
  assert.match(f.calls.at(-1).url, /offset=9900&limit=50&expected_revision=revision%3Aone$/);
  assert.deepEqual(m.state.search.items.map((r) => r.position), [11000]);
  assert.equal(m.state.search.offset, 9900); assert.equal(m.state.search.complete, true);
  assert.equal(await m.search('Track', first), false);
  assert.equal(await m.search('Track', m.state.search), false);
  assert.equal(f.calls.length, 3);
});

test('empty queues have complete zero-entry search coverage', async () => {
  const f = await fixture(0); f.answer(() => result({ total: 0, end: 0, positions: [] }));
  assert.equal(await f.model.search('Track'), true);
  assert.equal(f.model.state.search.examined, 0); assert.equal(f.model.state.search.complete, true);
  assert.equal(await f.model.jump(1), false); assert.equal(f.model.state.search.items.length, 0);
});

for (const [label, value] of [['blank', '  '], ['control', 'Track\nartist'], ['control range', 'Track\u0085artist'],
  ['word limit', Array(33).fill('word').join(' ')], ['byte limit', '😀'.repeat(257)], ['object', {}]]) {
  test(`invalid ${label} query sends no search read`, async () => {
    const f = await fixture(); assert.equal(await f.model.search(value), false);
    assert.equal(f.calls.length, 1); assert.match(f.model.state.searchError, /1–32/);
  });
}

test('queries and opaque revisions are escaped without becoming URL syntax', async () => {
  const f = await fixture(); const query = 'Prélude & movement / 2?';
  f.answer((url) => url.includes('/search?')
    ? result({ query, revision: 'r:/?&' }) : page(0, 120, 'r:/?&'));
  await f.model.read(); assert.equal(await f.model.search(query), true);
  const u = new URL(f.calls.at(-1).url, 'https://fixture');
  assert.equal(u.searchParams.get('q'), query); assert.equal(u.searchParams.get('expected_revision'), 'r:/?&');
  assert.equal([...u.searchParams.keys()].length, 4);
});

const malformed = [
  ['zone', (v) => ({ ...v, zone: 'Kitchen' })],
  ['query', (v) => ({ ...v, query: 'Different' })],
  ['revision', (v) => ({ ...v, revision: 'new' })],
  ['total', (v) => ({ ...v, total: 121 })],
  ['offset', (v) => ({ ...v, offset: 1 })],
  ['negative coverage', (v) => ({ ...v, examined: -1 })],
  ['coverage overflow', (v) => ({ ...v, examined: 0x100000000 })],
  ['false completion', (v) => ({ ...v, complete: false, next_offset: 120 })],
  ['extra continuation', (v) => ({ ...v, next_offset: 120 })],
  ['zero progress', (v) => ({ ...v, examined: 0, complete: false, next_offset: 0, items: [] })],
  ['wrong continuation', (v) => ({ ...v, examined: 60, complete: false, next_offset: 61 })],
  ['repeated position', (v) => ({ ...v, items: [row(42), row(42)] })],
  ['out of order', (v) => ({ ...v, items: [row(44), row(42)] })],
  ['outside coverage', (v) => ({ ...v, items: [row(121)] })],
  ['excess matches', (v) => ({ ...v, items: Array.from({ length: 51 }, (_, i) => row(i + 1)) })],
  ['oversized metadata', (v) => ({ ...v, items: [{ ...row(42), album: '😀'.repeat(4097) }] })],
  ['inconsistent visible URI', (v) => ({ ...v, items: [{ ...row(2), uri: 'different' }] })],
  ['inconsistent visible metadata', (v) => ({ ...v, items: [{ ...row(2), title: 'Changed' }] })],
];
for (const [name, change] of malformed) test(`malformed ${name} search is rejected without an actionable result`, async () => {
  const f = await fixture(); f.answer(() => change(result()));
  assert.equal(await f.model.search('Track'), false);
  assert.equal(f.model.state.search, null); assert.equal(f.model.state.stale, true);
  assert.equal(await f.model.search('Track'), false); assert.equal(f.calls.length, 2);
});

test('opening a repeated occurrence rereads its exact page and leaves playback unchanged', async () => {
  const f = await fixture(), m = f.model;
  await m.search('Track'); const found = m.state.search, occurrence = found.items[1];
  assert.equal(await m.openMatch(occurrence, found), true);
  assert.match(f.calls.at(-1).url, /queue\?offset=40&limit=40$/);
  assert.equal(m.state.page.offset, 40); assert.equal(m.state.found, 44);
  assert.equal(m.state.selection, null); assert.equal(m.state.search, found);
  assert.ok(f.calls.every((c) => c.body === undefined));
  const choice = m.select(44), op = Q.plan(choice, 'play');
  assert.equal(op.body.position, 44); assert.equal(op.body.expected_uri, occurrence.uri);
  assert.equal(op.body.expected_revision, found.revision);
  await Q.verify(async (url) => page(Number(new URL(url, 'https://fixture').searchParams.get('offset'))), op);
});

test('cloned, stale and unrenderable results cannot select a destination', async () => {
  const f = await fixture(), m = f.model; await m.search('Track');
  const first = m.state.search, hit = first.items[0];
  for (const candidate of [{ ...hit }, row(1), null]) assert.equal(await m.openMatch(candidate), false);
  assert.equal(await m.openMatch(hit, { ...first }), false);
  await m.search('Track'); assert.equal(await m.openMatch(hit, first), false);
  f.answer(() => result({ positions: [42], ...{} }));
  await m.search('Track');
  f.answer(() => ({ ...result(), items: [{ ...row(42), uri: null }] }));
  await m.search('Track');
  assert.equal(await m.openMatch(m.state.search.items[0]), false);
  assert.equal(m.state.found, null);
});

for (const change of ['revision', 'total', 'identity', 'metadata']) test(`opening rejects changed ${change} without a fallback or a write`, async () => {
  const f = await fixture(), m = f.model; await m.search('Track');
  const found = m.state.search.items[0], before = m.state.page;
  f.answer(() => {
    const p = page(40, change === 'total' ? 119 : 120, change === 'revision' ? 'new' : 'revision:one');
    if (change === 'identity') p.items[1].uri = 'changed';
    if (change === 'metadata') p.items[1].title = 'changed';
    return p;
  });
  assert.equal(await m.openMatch(found), false);
  assert.equal(m.state.page, before); assert.equal(m.state.stale, true);
  assert.equal(m.state.search, null); assert.equal(m.state.found, null);
  assert.match(m.state.error, /changed/); assert.equal(f.calls.length, 3);
  assert.ok(f.calls.every((c) => c.body === undefined));
});

test('search retires a reviewed queue choice and serializes all queue reads', async () => {
  const f = await fixture(), m = f.model, choice = m.select(2), d = deferred();
  f.answer(() => d.promise);
  const searching = m.search('Track');
  assert.equal(m.state.loading, true); assert.equal(m.state.searching, true);
  assert.throws(() => Q.plan(choice, 'play'));
  assert.equal(m.select(2), null); assert.equal(m.navigate(1), false);
  assert.equal(await m.read(), false); assert.equal(await m.search('Other'), false);
  assert.equal(await m.jump(42), false); assert.equal(f.calls.length, 2);
  d.resolve(result()); assert.equal(await searching, true);
});

test('cancel aborts only its read and fences a late reply even if transport ignores abort', async () => {
  const f = await fixture(), m = f.model, d = deferred(); f.answer(() => d.promise);
  const pending = m.search('Track'); const signal = f.calls.at(-1).options.signal;
  assert.equal(m.cancelSearch(), true); assert.equal(signal.aborted, true);
  assert.equal(await m.search('Track'), false); assert.equal(await m.read(), false);
  d.resolve(result()); assert.equal(await pending, false);
  assert.equal(m.state.search, null); assert.equal(m.state.loading, false); assert.equal(m.state.searching, false);
  assert.equal(m.state.searchError, 'Search cancelled.'); assert.equal(f.calls.length, 2);
});

test('editing a query retires results and cancels a pending search', async () => {
  const f = await fixture(), m = f.model, d = deferred(); await m.search('Track');
  const old = m.state.search, hit = old.items[0];
  m.setQuery('Other'); assert.equal(m.state.search, null); assert.equal(await m.openMatch(hit, old), false);
  f.answer(() => d.promise); const pending = m.search('Other');
  m.setQuery('Third'); d.resolve(result({ query: 'Other' }));
  assert.equal(await pending, false); assert.equal(m.state.query, 'Third'); assert.equal(m.state.search, null);
});

for (const action of ['close', 'suspend', 'dispose', 'invalidate', 'unfresh', 'household', 'coordinator', 'members']) {
  test(`${action} fences a late search and retires every occurrence`, async () => {
    const f = await fixture(), m = f.model, d = deferred(); f.answer(() => d.promise);
    const pending = m.search('Track'), signal = f.calls.at(-1).options.signal;
    if (['close', 'suspend', 'dispose', 'invalidate'].includes(action)) m[action]();
    else if (action === 'unfresh') m.observe({ ...f.item, fresh: false });
    else {
      const z = structuredClone(zone);
      if (action === 'household') z.household = 'S1';
      if (action === 'coordinator') z.coordinator_room = 'Kitchen';
      if (action === 'members') z.members.push('Another room');
      m.observe({ zone: z, fresh: true });
    }
    assert.equal(signal.aborted, true); d.resolve(result()); assert.equal(await pending, false);
    assert.equal(m.state.search, null); assert.equal(m.state.found, null); assert.equal(f.calls.length, 2);
  });
}

test('an event during destination navigation leaves the page stale and cannot highlight a result', async () => {
  const f = await fixture(), m = f.model; await m.search('Track');
  const found = m.state.search.items[0], d = deferred(); f.answer(() => d.promise);
  const reading = m.openMatch(found); m.invalidate(); d.resolve(page(40));
  assert.equal(await reading, false); assert.equal(m.state.found, null);
  assert.equal(m.state.stale, true); assert.equal(m.state.search, null);
});

test('query edits during result navigation cannot revive a former result', async () => {
  const f = await fixture(), m = f.model; await m.search('Track');
  const found = m.state.search.items[0], d = deferred(); f.answer(() => d.promise);
  const reading = m.openMatch(found); m.setQuery('Different'); d.resolve(page(40));
  assert.equal(await reading, false); assert.equal(m.state.found, null); assert.equal(m.state.stale, true);
});

test('a failed search preserves the last page as stale and includes native failure guidance', async () => {
  const f = await fixture(), before = f.model.state.page;
  f.answer(() => { throw Object.assign(new Error('Queue changed'), { hint: 'Start again at offset zero.' }); });
  assert.equal(await f.model.search('Track'), false);
  assert.equal(f.model.state.page, before); assert.equal(f.model.state.stale, true);
  assert.match(f.model.state.searchError, /Start again/); assert.equal(f.calls.length, 2);
});

test('ordinary pagination preserves search only while revision and total still match', async () => {
  const f = await fixture(), m = f.model; await m.search('Track'); const found = m.state.search;
  await m.read(40); assert.equal(m.state.search, found);
  f.answer(() => page(0, 120, 'revision:two'));
  await m.read(0); assert.equal(m.state.search, null); assert.equal(m.state.stale, false);
});

test('direct jumps open a revision-bound page without choosing or playing an entry', async () => {
  const f = await fixture(), m = f.model; const choice = m.select(2);
  assert.equal(await m.jump(99), true); assert.equal(m.state.page.offset, 80); assert.equal(m.state.found, 99);
  assert.equal(m.state.selection, null); assert.throws(() => Q.plan(choice, 'play'));
  assert.equal(f.calls.length, 2); assert.ok(f.calls.every((c) => c.body === undefined));
  for (const position of [0, -1, 121, 1.5, '2', NaN, Infinity]) assert.equal(await m.jump(position), false);
  assert.equal(f.calls.length, 2);
});

test('the final representable native position uses a non-overflowing page size', async () => {
  const f = await fixture(0xffffffff); assert.equal(await f.model.jump(0xffffffff), true);
  assert.match(f.calls.at(-1).url, /offset=4294967280&limit=15$/);
  assert.equal(f.model.state.page.items.at(-1).position, 0xffffffff);
});

test('a page without a usable revision cannot search or jump', async () => {
  const f = await fixture(); f.answer(() => ({ ...page(), revision: undefined })); await f.model.read();
  assert.equal(await f.model.search('Track'), false); assert.equal(await f.model.jump(2), false);
  assert.equal(f.calls.length, 2);
});

class Element {
  constructor(tag, cls = '', text = '') {
    this.tagName = tag; this.className = cls; this.textContent = text; this.children = [];
    this.dataset = {}; this.attrs = {}; this.events = {}; this.value = ''; this.disabled = false;
  }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  setAttribute(key, value) { this.attrs[key] = String(value); }
  addEventListener(type, fn) { (this.events[type] ||= []).push(fn); }
  contains(node) { return this === node || this.children.some((child) => child.contains?.(node)); }
  querySelectorAll(tag) { return this.children.flatMap((child) => [...(child.tagName === tag ? [child] : []), ...child.querySelectorAll(tag)]); }
  focus() { global.document.activeElement = this; }
  dispatch(type) { const event = { preventDefault() {} }; for (const fn of this.events[type] || []) fn(event); }
}
function all(node) { return [node, ...node.children.flatMap(all)]; }

test('finder shows exact coverage, opens duplicates through ordinary queue controls and uses text nodes', async (t) => {
  const before = global.document; global.document = { hidden: false, activeElement: null };
  t.after(() => { global.document = before; });
  const f = await fixture(), controller = { state: { busy: false } }, session = { state: { pending: null } };
  const finder = Q.finder({ queue: f.model, controller, session, el: (tag, cls, text) => new Element(tag, cls, text) });
  f.changed(() => finder.update());
  const controls = Object.fromEntries(all(finder.node).filter((n) => n.dataset.queueFind).map((n) => [n.dataset.queueFind, n]));
  controls.query.value = 'Track'; controls.query.dispatch('input');
  const forms = all(finder.node).filter((n) => n.tagName === 'form');
  forms[0].dispatch('submit'); await tick();
  const status = all(finder.node).find((n) => n.className === 'queue-find-status');
  assert.match(status.textContent, /2 matches/); assert.match(status.textContent, /entire observed queue/);
  const buttons = all(finder.node).filter((n) => n.dataset.queueMatch);
  assert.deepEqual(buttons.map((n) => n.dataset.queueMatch), ['42', '44']);
  buttons[1].dispatch('click'); await tick();
  assert.equal(f.model.state.found, 44); assert.equal(f.model.state.selection, null);
  assert.ok(f.calls.every((c) => c.body === undefined));
  controller.state.busy = true; finder.update();
  assert.equal(controls.submit.disabled, true); assert.equal(controls.jump.disabled, true);
  controller.state.busy = false; session.state.pending = {}; finder.update();
  assert.equal(controls.submit.disabled, true);
});

test('finder never labels a continued section as the whole queue and blocks forged result handlers', async (t) => {
  const before = global.document; global.document = { hidden: false, activeElement: null };
  t.after(() => { global.document = before; });
  const f = await fixture(200);
  f.answer((url) => result({ total: 200, offset: Number(new URL(url, 'https://fixture').searchParams.get('offset')),
    end: Number(new URL(url, 'https://fixture').searchParams.get('offset')) ? 200 : 100, positions: Number(new URL(url, 'https://fixture').searchParams.get('offset')) ? [150] : [42] }));
  const finder = Q.finder({ queue: f.model, controller: { state: { busy: false } },
    session: { state: { pending: null } }, el: (tag, cls, text) => new Element(tag, cls, text) });
  f.changed(() => finder.update());
  await f.model.search('Track');
  const oldButton = all(finder.node).find((n) => n.dataset.queueMatch === '42');
  all(finder.node).find((n) => n.dataset.queueFind === 'more').dispatch('click'); await tick();
  const status = all(finder.node).find((n) => n.className === 'queue-find-status');
  assert.match(status.textContent, /101–200/); assert.match(status.textContent, /section reached the end/);
  assert.doesNotMatch(status.textContent, /entire observed/);
  oldButton.dispatch('click'); await tick(); assert.equal(f.calls.length, 3);
});
