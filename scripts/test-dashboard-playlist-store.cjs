'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 30; i++) await Promise.resolve(); };
const target = 'Living Room@S2', other = 'Living Room@S1';
const catalogPath = value => '/sonos/playlists?zone=' + encodeURIComponent(value);
const decode = value => {
  if (!Array.isArray(value) || value.some(item => !/^SQ:\d+$/.test(item.id) || typeof item.title !== 'string')) throw new Error('Unreadable saved catalog');
  return value;
};
async function fixture({ speakers = true, demo = false } = {}) {
  const zones = speakers ? ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Living Room', members: ['Living Room'], transport_state: 'playing' })) : [];
  const calls = []; let readHook, writeHook;
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) {
      if (writeHook) return writeHook(url, body, options);
      return { done: 'Saved native playlist', changed: true };
    }
    if (readHook) { const value = await readHook(url, options); if (value !== undefined) return value; }
    if (url === '/zones') return structuredClone(zones);
    const zone = zones.find(z => url === `/zones/${encodeURIComponent(C.key(z))}/state`);
    if (zone) return { zone: structuredClone(zone), transport_state: 'playing', track: null, volume: 20 };
    if (url.startsWith('/sonos/playlists?zone=')) return [{ id: 'SQ:8', title: 'Evening' }];
    if (url === '/schedules') return [];
    if (url === '/dj/preferences') return {};
    throw new Error('Unexpected route ' + url);
  };
  const controller = C.model({ call, demo, now: () => 1000 }); await controller.refresh();
  const save = operation => controller.mutate(operation || (({ send }) => send('/sonos/playlists/save', { zone: target, title: 'Evening' })),
    { stored: 'playlists', storeTarget: target });
  return { controller, calls, zones, save, writes: () => calls.filter(row => row.body !== undefined),
    reader(fn) { readHook = fn; }, writer(fn) { writeHook = fn; } };
}

test('saving uses the selected qualified target without refreshing or changing playback', async () => {
  const f = await fixture(); const before = f.calls.length, state = f.controller.selected().state;
  assert.equal(await f.save(async ({ item, target: scoped, send }) => {
    assert.equal(scoped, target); assert.equal(C.key(item.zone), target);
    return send('/sonos/playlists/save', { zone: scoped, title: 'Evening' });
  }), true);
  assert.equal(f.calls.length, before + 1); assert.equal(f.controller.selected().state, state);
  assert.equal(f.controller.selected().fresh, true); assert.equal(f.controller.state.uncertain, false);
});

test('native catalog writes require a fresh selected speaker and exact scope', async () => {
  const f = await fixture(); let entered = false;
  for (const storeTarget of [undefined, '', 'Living Room', '@S2', 'Living Room@', other, 'Other@S2']) {
    assert.equal(await f.controller.mutate(() => { entered = true; }, { stored: 'playlists', storeTarget }), false);
  }
  f.controller.invalidate([target]); assert.equal(await f.save(() => { entered = true; }), false);
  assert.equal(entered, false); assert.equal(f.writes().length, 0);
  for (const options of [{ speakers: false }, { demo: true }]) {
    const unavailable = await fixture(options); assert.equal(await unavailable.save(() => { entered = true; }), false);
  }
  assert.equal(entered, false);
});

test('the scoped lane only sends the native save POST for its captured room', async () => {
  for (const [url, body, options] of [
    ['/pause', { zone: target }], ['/sonos/playlists/play', { zone: target, playlist: 'SQ:8' }],
    ['/sonos/playlists/save?zone=' + target, { zone: target, title: 'Evening' }],
    ['/sonos/playlists/save', { zone: other, title: 'Evening' }],
    ['/sonos/playlists/save', null], ['/sonos/playlists/save', []],
    ['/sonos/playlists/save', { zone: target, title: 'Evening' }, { method: 'PUT' }],
    ['/sonos/playlists/save', { zone: target, title: 'Evening' }, { method: 'GET' }],
  ]) {
    const f = await fixture(); assert.equal(await f.save(({ send }) => send(url, body, options)), false);
    assert.equal(f.writes().length, 0); assert.equal(f.controller.state.uncertain, false);
    assert.match(f.controller.state.message, /No command was sent/);
  }
});

test('a catalog read requires a qualified original scope and uses its exact GET URL', async () => {
  const f = await fixture(); const before = f.calls.length;
  for (const storeTarget of [undefined, '', 'Living Room', '@S2', 'Living Room@', 'Living\nRoom@S2']) {
    assert.equal(await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget }), null);
  }
  assert.equal(f.calls.length, before);
  assert.deepEqual(await f.controller.refreshStored('playlists', decode, { storeTarget: target }), [{ id: 'SQ:8', title: 'Evening' }]);
  assert.equal(f.calls.at(-1).url, catalogPath(target)); assert.equal(f.calls.at(-1).body, undefined);
});

test('only explicit valid original-household catalog reads clear uncertain saves', async () => {
  const f = await fixture(); f.writer(() => { throw new Error('Reply lost'); });
  assert.equal(await f.save(), false); assert.equal(f.controller.state.uncertain, true);
  assert.equal(await f.controller.refresh(true), true); assert.equal(f.controller.state.uncertain, true);
  for (const store of ['schedules', 'preferences']) {
    await f.controller.refreshStored(store, value => value, { explicit: true }); assert.equal(f.controller.state.uncertain, true);
  }
  await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: other });
  assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('playlists', decode, { storeTarget: target }); assert.equal(f.controller.state.uncertain, true);
  f.reader(url => url === catalogPath(target) ? {} : undefined);
  await assert.rejects(f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target }), /Unreadable/);
  assert.equal(f.controller.state.uncertain, true); f.reader(null);
  assert.equal(f.controller.select(other), true);
  await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target });
  assert.equal(f.controller.state.uncertain, false); assert.equal(f.controller.state.selected, other);
  assert.match(f.controller.state.message, /Living Room@S2/); assert.equal(f.writes().length, 1);
});

test('same-store reads do not acknowledge other mutations or another uncertain target', async () => {
  for (const stored of [undefined, 'schedules', 'preferences']) {
    const f = await fixture(); f.writer(() => { throw new Error('Reply lost'); });
    const route = stored === 'schedules' ? '/schedules' : stored === 'preferences' ? '/dj/preferences' : '/pause';
    await f.controller.mutate(({ send }) => send(route, { zone: target }), stored ? { stored } : {});
    await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target });
    assert.equal(f.controller.state.uncertain, true); assert.equal(f.writes().length, 1);
  }
});

test('group or lifecycle changes before dispatch cancel the native save without uncertainty', async () => {
  for (const change of [c => c.invalidate([target]), c => c.invalidate(), c => { c.suspend(); c.resume(); }]) {
    const f = await fixture(), gate = deferred();
    const saving = f.save(async ({ check, send }) => { await gate.promise; check(); return send('/sonos/playlists/save', { zone: target, title: 'Evening' }); });
    change(f.controller); gate.resolve(); assert.equal(await saving, false);
    assert.equal(f.writes().length, 0); assert.equal(f.controller.state.uncertain, false);
  }
});

test('one in-flight save excludes every mutation lane and cannot be replayed after close', async () => {
  const f = await fixture(), gate = deferred(); let captured;
  const saving = f.save(async ({ send }) => { captured = send; return gate.promise; });
  assert.equal(await f.save(), false); assert.equal(await f.controller.command('pause'), false);
  assert.equal(await f.controller.mutate(() => assert.fail('occupied lane'), { stored: 'preferences' }), false);
  assert.equal(f.controller.select(other), false);
  assert.equal(await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target }), null);
  gate.resolve({ done: 'Cancelled before dispatch', changed: false }); assert.equal(await saving, true);
  await assert.rejects(captured('/sonos/playlists/save', { zone: target, title: 'Evening' }), /view changed/);
  assert.equal(f.writes().length, 0);
});

test('a lost sent response retains its original scope across hide, reopen and selection changes', async () => {
  const f = await fixture(), gate = deferred(); f.writer(() => gate.promise);
  const saving = f.save(); await settle(); assert.equal(f.writes().length, 1);
  f.controller.suspend(); f.controller.resume(); gate.reject(new Error('Lost while hidden'));
  assert.equal(await saving, false); await f.controller.refresh(true); f.controller.select(other);
  assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: other }); assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target }); assert.equal(f.controller.state.uncertain, false);
  assert.equal(f.writes().length, 1);
});

test('a retired or aborted catalog observation cannot release the command lock', async () => {
  for (const retire of ['lifecycle', 'signal', 'decoder']) {
    const f = await fixture(), gate = deferred(), abort = new AbortController();
    f.writer(() => { throw new Error('Lost'); }); await f.save();
    f.reader(url => url === catalogPath(target) ? gate.promise : undefined);
    const reading = f.controller.refreshStored('playlists', value => {
      if (retire === 'decoder') f.controller.suspend(); return decode(value);
    }, { explicit: true, storeTarget: target, signal: abort.signal });
    if (retire === 'lifecycle') { f.controller.suspend(); f.controller.resume(); }
    if (retire === 'signal') abort.abort();
    gate.resolve([{ id: 'SQ:8', title: 'Evening' }]); assert.equal(await reading, null);
    assert.equal(f.controller.state.uncertain, true);
  }
});

test('an old explicit observation cannot clear a newer uncertain save to the same household', async () => {
  const f = await fixture(), gate = deferred(); let count = 0;
  f.writer(() => { throw new Error('Lost'); }); await f.save();
  f.reader(url => url === catalogPath(target) && ++count === 1 ? gate.promise : undefined);
  const oldRead = f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target });
  await f.controller.refreshStored('playlists', decode, { explicit: true, storeTarget: target });
  assert.equal(f.controller.state.uncertain, false); await f.save(); assert.equal(f.controller.state.uncertain, true);
  gate.resolve([{ id: 'SQ:8', title: 'Evening' }]); await oldRead;
  assert.equal(f.controller.state.uncertain, true); assert.equal(f.writes().length, 2);
});

test('house settings remain editable without speakers and reject an accidental target scope', async () => {
  const f = await fixture({ speakers: false });
  assert.equal(await f.controller.mutate(({ send }) => send('/dj/preferences', { key: 'energy', value: '50' }), { stored: 'preferences' }), true);
  assert.equal(await f.controller.mutate(() => assert.fail('wrong scope'), { stored: 'preferences', storeTarget: target }), false);
  assert.equal(await f.controller.refreshStored('preferences', value => value, { explicit: true, storeTarget: target }), null);
  assert.equal(f.writes().length, 1);
});
