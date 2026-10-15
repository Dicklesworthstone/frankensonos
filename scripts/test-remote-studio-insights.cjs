// node --test scripts/test-remote-studio-insights.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { model } = require('../crates/fsonos-api/src/remote/studio-insights.js');
const settle = async () => { for (let i = 0; i < 50; i++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const target = (home = 'S2', room = 'Den', members = [room]) => ({
  room, target: `${room}@${home}`, label: `${members.join(' + ')} (${home})`,
  signature: JSON.stringify([home, room, [...members].sort()]),
});
const observation = (zone = 'Den', title = 'Suite for a quiet evening') => ({
  zone, running: true, steering: { source: 'program', mood: 'dinner' },
  now: { title, composer: 'Example artist', movements: 3, movement: 2,
    movement_title: 'Second movement', reason: { summary: 'A complete work selected for variety.', factors: [] } },
  next: [{ title: 'The next work', composer: 'Another artist', movements: 1, reason: { summary: 'Variety' } }],
});
function harness(options = {}) {
  const calls = [], published = [];
  let now = 10000, read = async selected => observation(selected.room);
  const control = model({
    read: selected => { calls.push(selected); return read(selected); },
    validate: (value, selected) => {
      if (!value || value.zone !== selected.room || typeof value.running !== 'boolean' || !value.steering) throw new Error('Invalid DJ observation');
      return structuredClone(value);
    },
    now: () => now,
    changed: state => published.push(structuredClone(state)),
    ...options,
  });
  return { control, calls, published, readWith(fn) { read = fn; }, clock(value) { now = value; } };
}

test('a chosen group reads its qualified address without any room lookup or write', async () => {
  const h = harness(); h.control.select(target('S1', 'Den / West')); await settle();
  assert.equal(h.calls.length, 1); assert.equal(h.calls[0].target, 'Den / West@S1');
  assert.equal(h.control.state.value.zone, 'Den / West'); assert.equal(h.control.state.stale, false);
});
test('repeated layout observations never repeatedly read an unchanged selection', async () => {
  const h = harness(); h.control.select(target()); await settle();
  for (let n = 0; n < 50; n++) h.control.select(target());
  await settle(); assert.equal(h.calls.length, 1);
});
test('same-name rooms in another household reset the previous story before reading', async () => {
  const h = harness(); h.control.select(target('S1')); await settle();
  const waiting = deferred(); h.readWith(() => waiting.promise); h.control.select(target('S2'));
  assert.equal(h.control.state.value, null); assert.equal(h.control.state.at, null);
  await settle(); assert.deepEqual(h.calls.map(c => c.target), ['Den@S1', 'Den@S2']);
  waiting.resolve(observation('Den', 'Second household')); await settle();
  assert.equal(h.control.state.value.now.title, 'Second household');
});
test('a topology change invalidates a story even when its coordinator name did not change', async () => {
  const h = harness(); h.control.select(target()); await settle();
  h.control.select(target('S2', 'Den', ['Den', 'Kitchen'])); assert.equal(h.control.state.value, null);
  await settle(); assert.equal(h.calls.length, 2);
});
test('an old response cannot publish over a new selection and reads remain serialized', async () => {
  const h = harness(), old = deferred(), fresh = deferred();
  h.readWith(selected => selected.room === 'Den' ? old.promise : fresh.promise);
  h.control.select(target()); await settle(); h.control.select(target('S2', 'Office')); await settle();
  assert.equal(h.calls.length, 1, 'the selected read waits for the existing request');
  old.resolve(observation('Den', 'Outdated')); await settle();
  assert.equal(h.control.state.value, null); assert.equal(h.calls.length, 2);
  fresh.resolve(observation('Office', 'Current')); await settle();
  assert.equal(h.control.state.value.now.title, 'Current');
  assert.equal(h.published.some(p => p.value?.now.title === 'Outdated'), false);
});
test('a refresh burst has at most one follow-up read, with no overlap', async () => {
  const h = harness(), waits = [deferred(), deferred()]; let count = 0;
  h.readWith(() => waits[count++].promise);
  h.control.select(target()); await settle();
  for (let n = 0; n < 100; n++) void h.control.refresh();
  assert.equal(h.calls.length, 1); waits[0].resolve(observation()); await settle();
  assert.equal(h.calls.length, 2); waits[1].resolve(observation()); await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.control.state.loading, false);
});
test('failed reads retain the original observation time and visibly mark the old story', async () => {
  const h = harness(); h.control.select(target()); await settle();
  h.clock(80000); h.readWith(async () => { throw Object.assign(new Error('Unreachable'), { hint: 'Check the coordinator.' }); });
  await h.control.refresh();
  assert.equal(h.control.state.at, 10000); assert.equal(h.control.state.stale, true);
  assert.equal(h.control.state.error, 'Unreachable Check the coordinator.');
  assert.ok(h.control.state.value); assert.equal(h.control.state.loading, false);
});
test('a later successful refresh updates observation time and clears the error', async () => {
  const h = harness(); h.readWith(async () => { throw new Error('offline'); });
  h.control.select(target()); await settle(); h.clock(20000); h.readWith(async () => observation());
  await h.control.refresh(); assert.equal(h.control.state.at, 20000);
  assert.equal(h.control.state.error, ''); assert.equal(h.control.state.stale, false);
});
test('invalid or wrong-coordinator replies are not promoted to fresh state', async () => {
  for (const value of [null, {}, observation('Another room'), { ...observation(), running: 'yes' }]) {
    const h = harness(); h.readWith(async () => value); h.control.select(target()); await settle();
    assert.equal(h.control.state.value, null); assert.equal(h.control.state.stale, true);
    assert.match(h.control.state.error, /Invalid/); assert.equal(h.control.state.loading, false);
  }
});
test('hiding during a read fences the response and defers the next read until visible', async () => {
  const h = harness(), old = deferred(); h.readWith(() => old.promise);
  h.control.select(target()); await settle(); h.control.visible(false);
  for (let n = 0; n < 20; n++) void h.control.refresh();
  old.resolve(observation()); await settle();
  assert.equal(h.calls.length, 1); assert.equal(h.control.state.value, null);
  h.readWith(async () => observation()); h.control.visible(true); await settle();
  assert.equal(h.calls.length, 2); assert.equal(h.control.state.stale, false);
});
test('switching hidden workspaces issues no reads until Listen is opened', async () => {
  const h = harness(); h.control.visible(false); h.control.select(target()); await settle();
  assert.equal(h.calls.length, 0); h.control.select(target('S1')); await settle();
  assert.equal(h.calls.length, 0); h.control.visible(true); await settle();
  assert.equal(h.calls.length, 1); assert.equal(h.calls[0].target, 'Den@S1');
});
test('hide and immediate resume still serialize behind the previous request', async () => {
  const h = harness(), old = deferred(); h.readWith(() => old.promise);
  h.control.select(target()); await settle(); h.control.visible(false); h.control.visible(true); await settle();
  assert.equal(h.calls.length, 1); old.resolve(observation()); await settle();
  assert.equal(h.calls.length, 2);
});
test('an invalidation fences an in-flight result without creating a polling loop', async () => {
  const h = harness(), waiting = deferred(); h.readWith(() => waiting.promise);
  h.control.select(target()); await settle(); h.control.stale();
  waiting.resolve(observation()); await settle();
  assert.equal(h.control.state.value, null); assert.equal(h.control.state.stale, true);
  assert.equal(h.calls.length, 1);
});
test('clearing the selected group cancels queued work and never reveals its old story', async () => {
  const h = harness(), waiting = deferred(); h.readWith(() => waiting.promise);
  h.control.select(target()); await settle(); void h.control.refresh(); h.control.select(null);
  waiting.resolve(observation()); await settle();
  assert.equal(h.calls.length, 1); assert.equal(h.control.state.target, null); assert.equal(h.control.state.value, null);
});
test('disposal fences late completion and prevents further publication or requests', async () => {
  const h = harness(), waiting = deferred(); h.readWith(() => waiting.promise);
  h.control.select(target()); await settle(); h.control.dispose(); const count = h.published.length;
  waiting.resolve(observation()); await settle(); h.control.visible(true); h.control.select(target('S1')); await h.control.refresh();
  assert.equal(h.published.length, count); assert.equal(h.calls.length, 1);
});
test('a synchronous read failure releases the single-flight lane', async () => {
  const h = harness(); h.readWith(() => { throw new Error('synchronous failure'); });
  h.control.select(target()); await settle(); assert.equal(h.control.state.loading, false);
  h.readWith(async () => observation()); await h.control.refresh();
  assert.equal(h.calls.length, 2); assert.equal(h.control.state.stale, false);
});
test('a rendering exception cannot strand the reader or prevent recovery', async () => {
  const h = harness({ changed: () => { throw new Error('view unavailable'); } });
  h.control.select(target()); await settle(); await h.control.refresh();
  assert.equal(h.calls.length, 2); assert.equal(h.control.state.loading, false);
});
test('the selected address is copied before awaiting and cannot be changed by a caller', async () => {
  const h = harness(), selected = target(); h.control.select(selected); selected.target = 'Unexpected@S1';
  await settle(); assert.equal(h.calls[0].target, 'Den@S2'); assert.ok(Object.isFrozen(h.control.state.target));
});
test('a stopped DJ remains an observed stopped DJ, never a fabricated selection', async () => {
  const h = harness(); h.readWith(async () => ({ zone: 'Den', running: false, steering: { source: 'none' } }));
  h.control.select(target()); await settle(); assert.equal(h.control.state.value.running, false);
  assert.equal(h.control.state.value.now, undefined); assert.equal(h.control.state.value.next, undefined);
});
test('the embedded adapter reuses the existing DJ contract and creates no write surface', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/studio-insights.js'), 'utf8');
  assert.ok(source.includes("call('sessionStatus', { params: { room: zone.target } })"));
  assert.ok(source.includes('validate: schema.statusOf')); assert.ok(source.includes('schema.zonesOf([zone])'));
  for (const forbidden of ['fetch(', 'EventSource(', 'setInterval(', "call('djStart'", "call('djSteer'", "call('volume'", '.innerHTML', 'eval(']) assert.equal(source.includes(forbidden), false, forbidden);
  assert.ok(source.includes('host.after(node)'));
});
