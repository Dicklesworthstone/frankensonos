// Whole works and albums can be arranged across native queue pages.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const zone = { household: 'S2', coordinator_room: 'Study / room', members: ['Study / room', 'Kitchen'] };
const tick = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
function page(offset = 0, limit = 40, total = 180, revision = 'queue:revision') {
  return { zone: zone.coordinator_room, total, revision, playing: 38,
    items: Array.from({ length: Math.min(limit, Math.max(0, total - offset)) }, (_, index) => {
      const position = offset + index + 1;
      return { position, title: `Movement ${position}`, creator: 'Ensemble', album: 'Whole work', uri: `same:recording-${position % 3}` };
    }) };
}
async function fixture(total = 180) {
  const calls = [], item = { zone: structuredClone(zone), fresh: true };
  const response = url => { const params = new URL(url, 'http://fixture').searchParams;
    return page(Number(params.get('offset')), Number(params.get('limit')), total); };
  let handle = response, render = () => {};
  const call = async (url, body) => { calls.push({ url, body }); return handle(url, body); };
  const model = Q.model({ call, changed: () => render() }); model.observe(item); model.open(); await tick();
  return { calls, item, call, model, response, answer(fn) { handle = fn; }, changed(fn) { render = fn; } };
}

test('one bounded native read selects 100 exact occurrences across pages without replacing the visible page', async () => {
  const f = await fixture(), visible = f.model.state.page;
  const choice = await f.model.selectRange(35, 134);
  assert.equal(Q.MAX_SELECTION, 100); assert.equal(f.model.state.page, visible);
  assert.equal(Q.choiceOf(choice), choice); assert.equal(choice.rows.length, 100);
  assert.equal(choice.rows[0].position, 35); assert.equal(choice.rows.at(-1).position, 134);
  assert.equal(new Set(choice.rows.map(row => row.uri)).size, 3);
  assert.equal(f.calls[1].url, '/zones/Study%20%2F%20room%40S2/queue?offset=34&limit=100');
  const operation = Q.plan(choice, 'move', 181);
  assert.equal(operation.resultPosition, 81); assert.equal(operation.body.expected_revision, visible.revision);
  assert.deepEqual(operation.body.expected_uris, choice.rows.map(row => row.uri));
  await Q.verify(f.call, operation);
  assert.equal(f.calls.at(-1).url, f.calls[1].url);
  assert.ok(f.calls.every(call => call.body === undefined));
});

test('a range entirely outside the visible page still reads and retains its exact absolute positions', async () => {
  const f = await fixture(), choice = await f.model.selectRange(121, 145);
  assert.deepEqual(choice.rows.map(row => row.position), Array.from({ length: 25 }, (_, index) => index + 121));
  assert.equal(f.model.state.page.offset, 0); assert.equal(Q.plan(choice, 'remove').body.position, 121);
  assert.match(f.calls.at(-1).url, /offset=120&limit=25$/);
});

test('page navigation retains the selected range while consuming its earlier review authority', async () => {
  const f = await fixture(), first = await f.model.selectRange(35, 134), old = Q.plan(first, 'remove');
  for (const offset of [40, 80, 120]) {
    assert.equal(f.model.navigate(1), true); await tick();
    assert.equal(f.model.state.page.offset, offset);
    const choice = Q.choiceOf(f.model.state.selection);
    assert.deepEqual(choice.rows, first.rows); assert.notEqual(choice, first);
    assert.equal(Q.plan(choice, 'move', 1).body.expected_uris.length, 100);
  }
  await assert.rejects(Q.verify(f.call, old), /no longer current/);
  assert.equal(await f.model.jump(35), true); assert.deepEqual(f.model.state.selection.rows, first.rows);
  const current = Q.plan(f.model.state.selection, 'remove');
  await Q.verify(f.call, current); assert.match(f.calls.at(-1).url, /offset=34&limit=100$/);
  assert.ok(f.calls.every(call => call.body === undefined));
});

test('refresh clears an unchanged retained range and cannot silently reuse its review', async () => {
  const f = await fixture(), choice = await f.model.selectRange(35, 134), operation = Q.plan(choice, 'remove');
  await f.model.read(); assert.equal(f.model.state.selection, null);
  await assert.rejects(Q.verify(f.call, operation), /no longer current/);
});

test('clearing a range while navigating cannot be undone by the late page reply', async () => {
  const f = await fixture(), choice = await f.model.selectRange(35, 134), d = deferred();
  f.answer(() => d.promise); assert.equal(f.model.navigate(1), true);
  f.model.clearSelection(); d.resolve(page(40)); await tick();
  assert.equal(f.model.state.selection, null); assert.throws(() => Q.choiceOf(choice));
  assert.equal(f.model.state.page.offset, 40); assert.equal(f.model.state.stale, false);
});

for (const changed of ['revision', 'total', 'selected metadata']) {
  test(`navigation drops the retained range when ${changed} changes`, async () => {
    const f = await fixture(), choice = await f.model.selectRange(35, 134);
    f.answer(url => { const value = f.response(url);
      if (changed === 'revision') value.revision = 'another:queue';
      else if (changed === 'total') value.total++;
      else value.items[2].title = 'Different occurrence';
      return value;
    });
    assert.equal(f.model.navigate(1), true); await tick();
    assert.equal(f.model.state.selection, null); assert.throws(() => Q.choiceOf(choice));
    assert.ok(f.calls.every(call => call.body === undefined));
  });
}

test('invalid or oversized range drafts retire the old selection without issuing a read', async () => {
  const f = await fixture();
  for (const [first, last] of [[0, 5], [2, 1], [1, 101], [150, 181], [1.5, 4], ['1', 3], [NaN, 2], [1, Infinity]]) {
    const choice = f.model.select(2), count = f.calls.length;
    assert.equal(await f.model.selectRange(first, last), null);
    assert.equal(f.calls.length, count); assert.equal(f.model.state.selection, null);
    assert.throws(() => Q.choiceOf(choice)); assert.match(f.model.state.error, /100 consecutive/);
  }
});

for (const change of ['revision', 'total', 'overlap', 'missing identity', 'short page']) {
  test(`a range read with changed ${change} leaves no actionable selection`, async () => {
    const f = await fixture();
    f.answer(url => { const value = f.response(url);
      if (change === 'revision') value.revision = 'changed:queue';
      else if (change === 'total') value.total++;
      else if (change === 'overlap') value.items[0].title = 'Changed visible title';
      else if (change === 'missing identity') value.items[80].uri = null;
      else value.items.pop();
      return value;
    });
    assert.equal(await f.model.selectRange(35, 134), null); assert.equal(f.model.state.selection, null);
    assert.equal(f.model.state.stale, true); assert.match(f.model.state.error, /No queue command was sent/);
    assert.ok(f.calls.every(call => call.body === undefined));
  });
}

for (const boundary of ['clearSelection', 'close', 'suspend', 'dispose', 'invalidate', 'unfresh', 'household']) {
  test(`${boundary} while reading a range fences its late selection and starts no replacement read`, async () => {
    const f = await fixture(), d = deferred(); f.answer(() => d.promise);
    const selecting = f.model.selectRange(35, 134);
    assert.equal(f.model.state.loading, true);
    assert.equal(await f.model.selectRange(1, 20), null); assert.equal(f.model.navigate(1), false);
    if (boundary === 'unfresh') { f.item.fresh = false; f.model.observe(f.item); }
    else if (boundary === 'household') f.model.observe({ zone: { ...zone, household: 'S1' }, fresh: false });
    else f.model[boundary]();
    d.resolve(page(34, 100)); assert.equal(await selecting, null);
    assert.equal(f.model.state.selection, null); assert.equal(f.calls.length, 2);
    assert.ok(f.calls.every(call => call.body === undefined));
  });
}

test('a final native range keeps the largest representable position without overflow', async () => {
  const f = await fixture(0xffffffff), choice = await f.model.selectRange(0xffffffff - 3, 0xffffffff);
  assert.equal(choice.rows.length, 4); assert.equal(choice.rows.at(-1).position, 0xffffffff);
  assert.match(f.calls.at(-1).url, /offset=4294967291&limit=4$/);
  assert.equal(Q.plan(choice, 'move', 1).body.position, 0xffffffff - 3);
});

class Element {
  constructor(tag, cls = '', text = '') {
    this.tagName = tag; this.className = cls; this.textContent = text; this.children = [];
    this.dataset = {}; this.attrs = {}; this.events = {}; this.value = ''; this.disabled = false;
  }
  append(...nodes) { this.children.push(...nodes); }
  setAttribute(key, value) { this.attrs[key] = String(value); }
  addEventListener(type, fn) { (this.events[type] ||= []).push(fn); }
  focus() { global.document.activeElement = this; }
  dispatch(type) { for (const fn of this.events[type] || []) fn({ preventDefault() {} }); }
}
const all = node => [node, ...node.children.flatMap(all)];

test('the editor reads an off-page range, keeps its inputs while paging and reviews every selected occurrence', async t => {
  const before = global.document; global.document = { hidden: false, activeElement: null };
  t.after(() => { global.document = before; });
  const f = await fixture(), session = { state: { pending: null },
    reviewQueue(choice, action, destination) { return this.state.pending = Q.plan(choice, action, destination); } };
  const editor = Q.editor({ queue: f.model, controller: { state: { busy: false }, canControl: () => true }, session,
    el: (tag, cls, text) => new Element(tag, cls, text) });
  f.changed(() => editor.update());
  const fields = Object.fromEntries(all(editor.node).filter(node => node.dataset.queueField).map(node => [node.dataset.queueField, node]));
  const buttons = Object.fromEntries(all(editor.node).filter(node => node.dataset.queueEdit).map(node => [node.dataset.queueEdit, node]));
  const status = all(editor.node).find(node => node.className === 'queue-edit-status');
  fields.first.value = '35'; fields.first.dispatch('input'); fields.last.value = '134'; fields.last.dispatch('input');
  assert.equal(f.model.state.selection, null); assert.equal(buttons.remove.disabled, true); assert.equal(buttons.range.disabled, false);
  buttons.range.dispatch('click'); await tick();
  assert.equal(f.model.state.selection.rows.length, 100); assert.equal(buttons.remove.disabled, false);
  assert.match(status.textContent, /94 outside this page/);
  f.model.navigate(1); await tick();
  assert.equal(fields.first.value, '35'); assert.equal(fields.last.value, '134');
  assert.match(status.textContent, /60 outside this page/);
  buttons.remove.dispatch('click'); assert.equal(session.state.pending.body.expected_uris.length, 100);
  assert.equal(session.state.pending.body.position, 35); assert.ok(f.calls.every(call => call.body === undefined));
  session.state.pending = null; buttons.clear.dispatch('click');
  assert.equal(f.model.state.selection, null); assert.equal(fields.first.value, ''); assert.equal(fields.last.value, '');
});
