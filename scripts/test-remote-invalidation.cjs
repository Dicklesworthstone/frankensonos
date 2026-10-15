'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
// Exercise the shipped controller, not a second implementation of it.
const {join} = require('node:path');
const script = fs.readFileSync(join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
const begin = script.indexOf('const LiveRemote = (() => {');
const end = script.indexOf('\nfunction listen()', begin);
assert.ok(begin >= 0 && end > begin, 'the real reconciliation component must exist');
const source = script.slice(begin, end);
const live = vm.runInNewContext(source + '\nLiveRemote;', {setTimeout, clearTimeout});
const settle = () => new Promise(resolve => setImmediate(resolve));
async function fixture() {
  let id = 0;
  const timers = new Map(), handlers = new Map(), reads = [];
  const feed = {readyState: 1, addEventListener(name, fn) {handlers.set(name, fn);}, close() {}};
  const model = live.model({connect: () => feed,
    reload: async () => {reads.push('all');}, refresh: async room => {reads.push(room);},
    schedule(fn, delay) {timers.set(++id, {fn, delay}); return id;}, cancel(id) {timers.delete(id);},
  });
  const flush = async () => {
    assert.equal(timers.size, 1);
    const [id, task] = [...timers][0];
    assert.equal(task.delay, live.COALESCE);
    timers.delete(id); task.fn(); await settle();
  };
  const emit = (name, value) => handlers.get(name)({data: JSON.stringify(value)});
  model.resume(); emit('open', {}); await flush();
  assert.deepEqual(reads, ['all']); reads.length = 0;
  return {model, reads, timers, emit, flush};
}

test('same-URI metadata-only deltas coalesce into a fresh room read', async () => {
  const f = await fixture();
  for (let i = 0; i < 100; i++) f.emit('zone.state', {player: 'test', room: 'Den', metadata_changed: true});
  assert.equal(f.reads.length, 0);
  await f.flush(); assert.deepEqual(f.reads, ['Den']);
  assert.equal(f.timers.size, 0, 'no continuing poll needed'); f.model.dispose();
});

test('quiet upstream reset forces one whole-house read with no next event', async () => {
  const f = await fixture();
  f.emit('events.reset', {oldest: 9}); await f.flush();
  assert.deepEqual(f.reads, ['all']); assert.equal(f.timers.size, 0); f.model.dispose();
});

test('a reset supersedes queued metadata reads without replaying an action', async () => {
  const f = await fixture();
  f.emit('zone.state', {room: 'Den', metadata_changed: true});
  f.emit('events.reset', {oldest: 9});
  f.emit('zone.state', {room: 'Office', metadata_changed: true});
  await f.flush(); assert.deepEqual(f.reads, ['all']); f.model.dispose();
});

test('missing metadata room identity forces a full read, never a guessed target', async () => {
  const f = await fixture(); f.emit('zone.state', {player: 'unknown', metadata_changed: true});
  await f.flush(); assert.deepEqual(f.reads, ['all']); f.model.dispose();
});

test('metadata from duplicate room names invalidates both qualified groups', () => {
  const zones = [{household:'S1', coordinator_room:'Den', members:['Den']},
    {household:'S2',coordinator_room:'Den',members:['Den']},
    {household:'S2',coordinator_room:'Office',members:['Office']}];
  assert.deepEqual(live.affected(zones, 'Den'), zones.slice(0,2));
});
