// node --test scripts/test-remote-volume.cjs; executes the shipped room controls.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
const settle = async () => { for (let n = 0; n < 40; n++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
class Element {
  value = ''; disabled = false; textContent = ''; children = []; attrs = {}; handlers = {};
  setAttribute(key, value) { this.attrs[key] = value; if (key === 'value') this.value = value; if (key === 'disabled') this.disabled = true; }
  removeAttribute(key) { delete this.attrs[key]; }
  addEventListener(key, fn) { this.handlers[key] = fn; }
  append(child) { this.children.push(child); }
  emit(key) { this.handlers[key]?.(); }
}
function harness() {
  let now = 0, id = 0;
  const timers = new Map(), writes = [], reads = [], messages = [];
  let read = async () => ({ volume: 21, mute: false });
  const context = { document: { createElement: () => new Element() },
    Date: { now: () => now },
    setTimeout(fn, ms) { timers.set(++id, { fn, at: now + ms }); return id; },
    clearTimeout(key) { timers.delete(key); },
    network: (name, options) => {
      if (name === 'state') { reads.push(options.params.room); return read(); }
      const d = deferred(); writes.push({ name, body: JSON.parse(JSON.stringify(options.body)), ...d }); return d.promise;
    },
    message: (text) => messages.push(text),
  };
  const api = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\ncall = network; toast = message; ({house, slider, setVolume, refreshVolume, disposeCard, zoneKey});', context);
  function room(name = 'Den', household = 'S2') {
    const parts = { disposed: false, readVersion: 0, sliders: new Map(), queue: { dispose() {} },
      zone: { household, coordinator_room: name, members: [name] } };
    api.house.cards.set(api.zoneKey(parts.zone), parts);
    api.slider(parts, name, household);
    const entry = parts.sliders.get(name);
    api.setVolume(entry, 20);
    return { parts, entry, drag(value) { entry.input.value = String(value); entry.input.emit('input'); } };
  }
  return { api, room, writes, reads, messages, timers, readWith(fn) { read = fn; },
    async tick(ms) {
      const end = now + ms;
      for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}

test('a slider burst sends only the final absolute level to its qualified room', async () => {
  const h = harness(), r = h.room();
  r.drag(25); await h.tick(100); r.drag(30); await h.tick(249);
  assert.equal(h.writes.length, 0); await h.tick(1);
  assert.deepEqual(h.writes[0].body, { zone: 'Den@S2', volume: 30 });
});

test('slow volume writes serialize and coalesce every intervening draft', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250);
  r.drag(40); await h.tick(250); r.drag(15); await h.tick(250);
  assert.equal(h.writes.length, 1, 'never overlap writes to the same room');
  h.writes[0].resolve({ volume: 30 }); await settle();
  assert.equal(h.writes.length, 2);
  assert.deepEqual(h.writes.map(w => w.body.volume), [30, 15]);
  h.writes[1].resolve({ volume: 15 }); await settle();
  assert.equal(r.entry.shown.textContent, '15');
});

test('completion during the debounce period does not send the unfinished draft', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250); r.drag(40);
  h.writes[0].resolve({ volume: 30 }); await settle();
  assert.equal(h.writes.length, 1); await h.tick(249); assert.equal(h.writes.length, 1);
  await h.tick(1); assert.equal(h.writes[1].body.volume, 40);
});

test('live reads cannot overwrite a desired volume during a long write', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250); r.drag(10); await h.tick(5000);
  h.api.setVolume(r.entry, 80);
  assert.equal(r.entry.input.value, '10');
  assert.equal(h.writes.length, 1);
});

test('policy clamps are authoritative and do not become an automatic volume ramp', async () => {
  const h = harness(), r = h.room();
  r.drag(80); await h.tick(250);
  h.writes[0].resolve({ volume: 25, notes: [{ detail: 'Clamped to 25 by house policy.' }] }); await settle();
  assert.equal(r.entry.input.value, '25'); assert.equal(h.writes.length, 1);
  assert.ok(h.messages.includes('Clamped to 25 by house policy.'));
  await h.tick(60000); assert.equal(h.writes.length, 1);
});

test('a lost write reply drops even a debounced queued change and only reads back', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250); r.drag(50); await h.tick(250);
  h.writes[0].reject(new Error('reply lost')); await settle(); await h.tick(60000);
  assert.equal(h.writes.length, 1); assert.equal(h.reads.length, 1);
  assert.equal(r.entry.input.value, '21');
  assert.ok(h.messages.some(m => /reply lost/.test(m)));
});

test('a failed write cancels an unexpired debounce and blocks edits until a good read', async () => {
  const h = harness(), r = h.room(), read = deferred(); h.readWith(() => read.promise);
  r.drag(30); await h.tick(250); r.drag(70);
  h.writes[0].reject(new Error('offline')); await settle();
  assert.equal(r.entry.input.disabled, true);
  r.drag(90); await h.tick(5000); assert.equal(h.writes.length, 1);
  read.resolve({ volume: 26 }); await settle();
  assert.equal(r.entry.input.disabled, false); assert.equal(r.entry.input.value, '26');
});

test('reads begun before a write completes cannot replace its acknowledged level', async () => {
  const h = harness(), r = h.room(), read = deferred(); h.readWith(() => read.promise);
  r.drag(30); await h.tick(250);
  const loading = h.api.refreshVolume(r.entry, r.parts);
  h.writes[0].resolve({ volume: 27 }); await settle();
  read.resolve({ volume: 80 }); await loading;
  assert.equal(r.entry.input.value, '27');
});

test('disposing a card cancels unsent drafts and fences delayed completion', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250); r.drag(70);
  h.api.disposeCard(r.parts); h.writes[0].resolve({ volume: 30 });
  await settle(); await h.tick(5000); assert.equal(h.writes.length, 1);
});

test('a replacement card cannot write over the previous card\'s in-flight volume', async () => {
  const h = harness(), old = h.room();
  old.drag(30); await h.tick(250); h.api.disposeCard(old.parts);
  const replacement = h.room();
  assert.equal(replacement.entry.input.disabled, true);
  replacement.drag(70); await h.tick(250); assert.equal(h.writes.length, 1);
  h.writes[0].resolve({ volume: 30 }); await settle();
  assert.equal(replacement.entry.input.disabled, false);
  replacement.drag(15); await h.tick(250); assert.equal(h.writes[1].body.volume, 15);
});

test('same-name rooms in different households have independent write lanes', async () => {
  const h = harness(), s1 = h.room('Den', 'S1'), s2 = h.room('Den', 'S2');
  s1.drag(30); s2.drag(40); await h.tick(250);
  assert.deepEqual(h.writes.map(w => w.body.zone).sort(), ['Den@S1', 'Den@S2']);
});

test('invalid levels never reach the API or enable an unreadable slider', async () => {
  const h = harness(), r = h.room();
  for (const value of [-1, 101, NaN, 1.5, Infinity]) { r.drag(value); await h.tick(250); }
  assert.equal(h.writes.length, 0);
  h.api.setVolume(r.entry, 101); assert.notEqual(r.entry.input.value, '101');
});

test('an identical queued level is consumed rather than stepping past the first clamp', async () => {
  const h = harness(), r = h.room();
  r.drag(80); await h.tick(250); r.drag(80);
  h.writes[0].resolve({ volume: 25 }); await settle(); await h.tick(1000);
  assert.equal(h.writes.length, 1); assert.equal(r.entry.input.value, '25');
});

test('a malformed success reply is uncertain and never drains queued writes', async () => {
  const h = harness(), r = h.room();
  r.drag(30); await h.tick(250); r.drag(50); await h.tick(250);
  h.writes[0].resolve(null); await settle();
  assert.equal(h.writes.length, 1); assert.equal(h.reads.length, 1);
  assert.equal(r.entry.input.value, '21');
});

test('failed reconciliation leaves a disabled control until live state recovers', async () => {
  const h = harness(), r = h.room(); h.readWith(async () => { throw new Error('offline'); });
  r.drag(30); await h.tick(250); h.writes[0].reject(new Error('lost')); await settle();
  assert.equal(r.entry.input.disabled, true); assert.equal(r.entry.shown.textContent, '?');
  h.api.setVolume(r.entry, null); assert.equal(r.entry.input.disabled, true);
  h.api.setVolume(r.entry, 24); assert.equal(r.entry.input.disabled, false);
  assert.equal(r.entry.shown.textContent, '24');
});
