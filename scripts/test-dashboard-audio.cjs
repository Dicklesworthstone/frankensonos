'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Audio = require('../crates/fsonos-api/src/remote/dashboard-audio.js');
const V = require('../crates/fsonos-api/src/remote/dashboard-visualizer.js');
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve = a; reject = b; }); return { resolve, reject, promise }; };
const tick = () => new Promise((r) => setImmediate(r));
function fixture() {
  const tracks = [], contexts = [], connections = [], publications = [];
  let pending = null, expire, failConnect = false;
  const stream = () => {
    const t = { stopped: false, stop() { this.stopped = true; }, addEventListener(name, fn) { this.ended = fn; } };
    tracks.push(t); return { getTracks: () => [t] };
  };
  class Context {
    constructor() { contexts.push(this); this.closed = false; this.destination = 'NEVER CONNECT'; }
    resume() { return Promise.resolve(); }
    close() { this.closed = true; return Promise.resolve(); }
    createMediaStreamSource() { return { connect: (node) => { if (failConnect) throw new Error('Graph failed'); connections.push(node); }, disconnect() {} }; }
    createAnalyser() { return { frequencyBinCount: 1024, fftSize: 2048, disconnect() {},
      getByteTimeDomainData: (a) => a.fill(150), getByteFrequencyData: (a) => a.fill(128) }; }
  }
  const model = Audio.create({ secure: true, Context, media: { getUserMedia: (options) => {
    assert.equal(options.video, false); return pending || Promise.resolve(stream());
  } }, changed: (s) => publications.push(s), schedule: (fn) => { expire = fn; return 1; }, cancel: () => {} });
  return { model, tracks, contexts, connections, publications, stream,
    pending(p) { pending = p; }, expire() { expire(); }, failConnect() { failConnect = true; } };
}
test('microphone is opt-in and creates no device, context or analysis before start', () => {
  const f = fixture(); assert.equal(f.model.state.status, 'idle'); assert.equal(f.model.sample(), null);
  assert.equal(f.contexts.length, 0); assert.equal(f.tracks.length, 0);
});
test('explicit start builds a local analyser without any connection to output', async () => {
  const f = fixture(); assert.equal(await f.model.start(), true); assert.equal(f.model.state.status, 'live');
  assert.equal(f.connections.length, 1); assert.notEqual(f.connections[0], 'NEVER CONNECT');
  const sample = f.model.sample(); assert.equal(sample.bins.length, 64); assert.ok(sample.energy > 0 && sample.energy < 1);
  assert.ok([...sample.bins].every((v) => v >= 0 && v <= 1)); f.model.stop();
});
test('stop releases every media track and closes its context', async () => {
  const f = fixture(); await f.model.start(); f.model.stop();
  assert.equal(f.tracks[0].stopped, true); assert.equal(f.contexts[0].closed, true); assert.equal(f.model.sample(), null);
});
test('a cancelled pending permission cannot leave a microphone active when later allowed', async () => {
  const f = fixture(), d = deferred(); f.pending(d.promise);
  const pending = f.model.start(); assert.equal(await f.model.start(), false); f.model.stop();
  d.resolve(f.stream()); assert.equal(await pending, false);
  assert.equal(f.tracks[0].stopped, true); assert.equal(f.contexts[0].closed, true); assert.equal(f.model.state.status, 'idle');
});
test('permission timeout releases context and stops a later grant', async () => {
  const f = fixture(), d = deferred(); f.pending(d.promise); const pending = f.model.start(); f.expire();
  assert.equal(await pending, false); assert.equal(f.model.state.status, 'error'); assert.match(f.model.state.message, /timed out/);
  d.resolve(f.stream()); await tick(); assert.equal(f.tracks[0].stopped, true); assert.equal(f.contexts[0].closed, true);
});
test('browser denial has an actionable message without losing ambient mode', async () => {
  const f = fixture(); f.pending(Promise.reject(Object.assign(new Error('Denied'), { name: 'NotAllowedError' })));
  assert.equal(await f.model.start(), false); assert.match(f.model.state.message, /permission was denied/);
  assert.equal(f.contexts[0].closed, true); assert.equal(f.model.sample(), null);
});
test('graph construction failure releases the already-granted microphone', async () => {
  const f = fixture(); f.failConnect(); assert.equal(await f.model.start(), false);
  assert.equal(f.tracks[0].stopped, true); assert.equal(f.contexts[0].closed, true);
});
test('device disconnection stops analysis and does not restart capture', async () => {
  const f = fixture(); await f.model.start(); f.tracks[0].ended();
  assert.equal(f.model.state.status, 'idle'); assert.match(f.model.state.message, /disconnected/);
  assert.equal(f.model.sample(), null); assert.equal(f.contexts.length, 1);
});
test('insecure contexts fail clearly without requesting permission', async () => {
  let requests = 0;
  const model = Audio.create({ secure: false, Context: class {}, media: { getUserMedia() { requests++; } } });
  assert.equal(await model.start(), false); assert.equal(requests, 0); assert.match(model.state.message, /localhost or HTTPS/);
});
test('disposal fences late grants and cannot be undone with start', async () => {
  const f = fixture(), d = deferred(); f.pending(d.promise); const pending = f.model.start(); f.model.dispose();
  const count = f.publications.length; d.resolve(f.stream()); await pending;
  assert.equal(f.publications.length, count); assert.equal(f.tracks[0].stopped, true); assert.equal(await f.model.start(), false);
});
test('reduced-motion and visibility stop the animation scheduler', () => {
  let frames = 0, cancelled = 0;
  const context = new Proxy({}, { get: (_, key) => key.startsWith('create') ? () => ({ addColorStop() {} }) : () => {}, set: () => true });
  const canvas = { getContext: () => context, width: 100, height: 100 };
  const v = V.create(canvas, { reduced: true, now: () => 0, raf: () => ++frames, caf: () => ++cancelled });
  v.visible(true); assert.equal(frames, 0); v.pause(false); assert.equal(frames, 1);
  v.visible(false); assert.equal(cancelled, 1); v.visible(true); assert.equal(frames, 2);
  v.dispose(); assert.equal(cancelled, 2); v.visible(true); assert.equal(frames, 2);
});
