'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const A = require('../crates/fsonos-api/src/remote/announcements.js');
const D = require('../crates/fsonos-api/src/remote/dashboard-announcements.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const ID = '0123456789abcdef0123456789abcdef', OTHER = 'fedcba9876543210fedcba9876543210';
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settle = async () => { for (let i = 0; i < 30; ++i) await Promise.resolve(); };
const groups = () => [
  { household: 'S2', coordinator_room: 'Main', members: ['Main', 'Side'], transport_state: 'playing' },
  { household: 'S1', coordinator_room: 'Main', members: ['Main'], transport_state: 'stopped' },
];
function job(phase = 'queued', patch = {}) {
  const terminal = ['completed', 'cancelled', 'failed'].includes(phase);
  return { id: ID, done: `Announcement ${phase}`, kind: 'speech', rooms: ['Main@S2'], phase, created_at: 100,
    finished_at: terminal ? 101 : null, cancel_requested: phase === 'cancelled', can_cancel: !terminal,
    progress: phase === 'queued' ? [] : [{ household: 'fixture-house', lead: 'RINCON_MAIN', rooms: ['Main'], phase: terminal ? 'complete' : phase }],
    result: terminal ? { done: 'Playback restored', clean: phase === 'completed', households: [
      { lead: 'Main', rooms: ['Main'], levels: [{ room: 'Main', volume: 25 }],
        outcome: phase === 'cancelled' ? 'cancelled' : phase === 'failed' ? 'failed' : 'finished',
        ...(phase === 'failed' ? { error: 'Playback stopped elsewhere', not_restored: ['Manual source retained'] } : {}) },
    ] } : null, error: null, ...patch };
}
function fixture({ jobs = [job()], maxReads = 240, completed } = {}) {
  let read = () => jobs, cancel = () => job('restoring', { cancel_requested: true, can_cancel: false }), serial = 0;
  const calls = [], timers = new Map();
  const c = A.jobModel({ maxReads, completed, setTimer(fn) { timers.set(++serial, fn); return serial; }, clearTimer(id) { timers.delete(id); },
    call: async (name, args) => { calls.push([name, args]); return copy(await (name === 'announcementJobs' ? read(args) : cancel(args))); } });
  return { c, calls, timers, read(fn) { read = fn; }, cancel(fn) { cancel = fn; },
    writes: () => calls.filter(([name]) => name === 'announcementCancel'),
    async tick() { const entry = timers.entries().next().value; assert.ok(entry, 'one visible observation is scheduled'); timers.delete(entry[0]); entry[1](); await settle(); },
  };
}

test('job receipts validate active phases and retain terminal restoration details', () => {
  for (const phase of ['queued', 'preparing', 'playing', 'restoring', 'completed', 'cancelled', 'failed']) {
    const parsed = A.jobOf(job(phase));
    assert.equal(parsed.phase, phase); assert.ok(Object.isFrozen(parsed)); assert.ok(Object.isFrozen(parsed.rooms));
  }
  const failed = A.jobOf(job('failed'));
  assert.deepEqual(failed.result.households[0].not_restored, ['Manual source retained']);
  const broken = job('failed', { result: null, error: { code: 'NOT_READY', detail: 'Speech unavailable', hint: 'Use a chime' } });
  assert.equal(A.jobOf(broken).error.hint, 'Use a chime');
});

test('malformed, contradictory and wrong-room job receipts are rejected', () => {
  const invalid = [null, {}, job('queued', { id: 'other' }), job('queued', { rooms: ['Main'] }),
    job('queued', { rooms: ['Main@S2', 'Main@S2'] }), job('queued', { can_cancel: false }),
    job('queued', { finished_at: 123 }), job('queued', { result: job('completed').result }),
    job('completed', { result: null }), job('completed', { error: { code: 'FAILED', detail: 'failed' } }),
    job('cancelled', { cancel_requested: false }), job('failed', { result: job('completed').result }),
    job('playing', { progress: [{ household: null, lead: 'RINCON_MAIN', rooms: [], phase: 'playing' }] }),
    job('completed', { rooms: ['Main@S2', 'Side@S2'] })];
  for (const value of invalid) assert.throws(() => A.jobOf(value));
  assert.throws(() => A.jobsOf([job(), job()])); assert.throws(() => A.jobsOf(Array(65).fill(job())));
  const partial = job('cancelled'); partial.result.households[0].not_restored = ['User chose a new source'];
  assert.throws(() => A.jobOf(partial));
  partial.phase = 'failed'; assert.equal(A.jobOf(partial).phase, 'failed', 'incomplete cleanup is not advertised as success');
});

test('admission binds the source kind and complete household-qualified room set', () => {
  const review = { body: { text: 'Dinner', rooms: ['Main@S2'] } };
  assert.equal(A.admittedJob(job(), review).id, ID);
  for (const bad of [job('queued', { rooms: ['Main@S1'] }), job('queued', { kind: 'chime' }),
    job('queued', { rooms: ['Main@S2', 'Side@S2'] })]) assert.throws(() => A.admittedJob(bad, review), /does not match/);
});

test('a queued room rename preserves job identity while accepting fresh completion labels', async () => {
  const f = fixture(); await f.c.setVisible(true);
  const renamed = job('completed');
  renamed.result.households[0].rooms = ['Renamed Main'];
  renamed.result.households[0].lead = 'Renamed Main';
  renamed.result.households[0].levels[0].room = 'Renamed Main';
  renamed.progress[0].rooms = ['Renamed Main'];
  f.read(() => [renamed]); assert.equal(await f.c.refresh(), true);
  assert.deepEqual(f.c.state.jobs[0].rooms, ['Main@S2']);
  assert.deepEqual(f.c.state.jobs[0].result.households[0].rooms, ['Renamed Main']);
  assert.equal(f.c.state.jobs[0].phase, 'completed'); assert.equal(f.timers.size, 0); f.c.dispose();
});

test('job observation is lazy, visible only, stops on terminal receipts and refreshes playback once', async () => {
  let finished = 0; const f = fixture({ completed: () => { ++finished; } });
  assert.equal(f.calls.length, 0); assert.equal(await f.c.refresh(), false);
  await f.c.setVisible(true); assert.equal(f.calls.length, 1); assert.equal(f.timers.size, 1);
  f.read(() => [job('playing')]); await f.tick(); assert.equal(f.c.state.jobs[0].phase, 'playing');
  f.read(() => [job('completed')]); await f.tick(); assert.equal(finished, 1); assert.equal(f.timers.size, 0);
  await f.c.refresh(); assert.equal(finished, 1); assert.equal(f.writes().length, 0); f.c.dispose();
});

test('job observation has a finite read budget and explicit refresh starts a new budget', async () => {
  const f = fixture({ maxReads: 3 }); await f.c.setVisible(true); await f.tick(); await f.tick();
  assert.equal(f.calls.length, 3); assert.equal(f.timers.size, 0); assert.match(f.c.state.message, /updates paused/);
  await f.c.refresh(); assert.equal(f.calls.length, 4); assert.equal(f.timers.size, 1); f.c.dispose();
});

test('hiding aborts obsolete GETs, clears timers and ignores late observations', async () => {
  const f = fixture(), late = deferred(); let signal;
  f.read(args => { signal = args.signal; return late.promise; });
  const open = f.c.setVisible(true); await settle(); assert.equal(f.c.state.loading, true);
  await f.c.setVisible(false); assert.equal(signal.aborted, true);
  late.resolve([job('playing')]); assert.equal(await open, false);
  assert.equal(f.c.state.jobs.length, 0); assert.equal(f.timers.size, 0);
  f.read(() => [job('playing')]); await f.c.setVisible(true); assert.equal(f.c.state.jobs.length, 1);
  await f.c.setVisible(false); assert.equal(f.timers.size, 0); f.c.dispose();
});

test('failed reads stop automatic observation and explicit refresh recovers', async () => {
  const f = fixture(); f.read(() => { throw new Error('Offline'); });
  assert.equal(await f.c.setVisible(true), false); assert.equal(f.timers.size, 0); assert.equal(f.c.state.stale, true);
  f.read(() => [job()]); assert.equal(await f.c.refresh(), true); assert.equal(f.c.state.stale, false); f.c.dispose();
});

test('identity changes and regressing completion are refused without enabling cancellation', async () => {
  for (const changed of [job('queued', { rooms: ['Main@S1'] }), job('queued', { created_at: 999 })]) {
    const f = fixture(); await f.c.setVisible(true); f.read(() => [changed]);
    assert.equal(await f.c.refresh(), false); assert.equal(f.c.state.stale, true);
    assert.equal(f.c.canCancel(f.c.state.jobs[0]), false); assert.equal(f.writes().length, 0); f.c.dispose();
  }
  const f = fixture({ jobs: [job('completed')] }); await f.c.setVisible(true); f.read(() => [job()]);
  assert.equal(await f.c.refresh(), false); assert.equal(f.c.state.jobs[0].phase, 'completed'); f.c.dispose();
});

test('an admitted job retires an older read and cannot be erased by its late empty reply', async () => {
  const f = fixture(), old = deferred(); f.read(() => old.promise);
  const reading = f.c.setVisible(true); await settle();
  f.c.accept(job(), { body: { text: 'Dinner', rooms: ['Main@S2'] } });
  old.resolve([]); assert.equal(await reading, false); assert.equal(f.c.state.jobs[0].id, ID); f.c.dispose();
});

test('cancellation is one explicit POST to the immutable job, even on double activation', async () => {
  const f = fixture(), reply = deferred(); await f.c.setVisible(true); f.cancel(() => reply.promise);
  const selected = f.c.state.jobs[0], first = f.c.cancel(selected);
  assert.equal(await f.c.cancel(selected), false); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.writes()[0], ['announcementCancel', { params: { id: ID }, body: {} }]);
  reply.resolve(job('restoring', { cancel_requested: true, can_cancel: false })); assert.equal(await first, true);
  assert.equal(f.c.state.jobs[0].phase, 'restoring'); assert.equal(f.c.canCancel(f.c.state.jobs[0]), false);
  f.read(() => [job('cancelled')]); await f.tick(); assert.equal(f.c.state.jobs[0].phase, 'cancelled');
  assert.equal(f.timers.size, 0); assert.equal(f.writes().length, 1); f.c.dispose();
});

test('a lost cancel reply is observed without replay and terminal status resolves uncertainty', async () => {
  const f = fixture(); await f.c.setVisible(true); f.cancel(() => { throw new Error('Connection lost'); });
  assert.equal(await f.c.cancel(f.c.state.jobs[0]), false); assert.ok(f.c.state.uncertain.has(ID));
  assert.equal(await f.c.cancel(f.c.state.jobs[0]), false); assert.equal(f.writes().length, 1);
  assert.match(f.c.state.message, /not repeated/); f.read(() => [job('cancelled')]); await f.tick();
  assert.equal(f.c.state.uncertain.size, 0); assert.equal(f.c.state.jobs[0].phase, 'cancelled'); assert.equal(f.writes().length, 1); f.c.dispose();
});

test('wrong-id, wrong-household and unacknowledged cancellation receipts remain uncertain', async () => {
  for (const response of [job('cancelled', { id: OTHER }), job('cancelled', { rooms: ['Main@S1'] }), job('playing')]) {
    const f = fixture(); await f.c.setVisible(true); f.cancel(() => response);
    assert.equal(await f.c.cancel(f.c.state.jobs[0]), false); assert.ok(f.c.state.uncertain.has(ID));
    assert.equal(f.c.state.jobs[0].phase, 'queued'); assert.equal(f.writes().length, 1); f.c.dispose();
  }
});

test('closing a panel never aborts cancellation or cleanup and a late receipt stays available', async () => {
  const f = fixture(), pending = deferred(); await f.c.setVisible(true); f.cancel(() => pending.promise);
  const work = f.c.cancel(f.c.state.jobs[0]); await f.c.setVisible(false);
  assert.equal(f.writes()[0][1].signal, undefined); pending.resolve(job('cancelled'));
  assert.equal(await work, true); assert.equal(f.c.state.jobs[0].phase, 'cancelled'); assert.equal(f.timers.size, 0); f.c.dispose();
});

test('disposal ignores late cancel presentation and prevents all further actions', async () => {
  const f = fixture(), pending = deferred(); await f.c.setVisible(true); f.cancel(() => pending.promise);
  const selected = f.c.state.jobs[0], work = f.c.cancel(selected); f.c.dispose(); pending.resolve(job('cancelled')); await work;
  assert.equal(f.c.state.jobs[0].phase, 'queued'); assert.equal(await f.c.cancel(selected), false);
  assert.equal(await f.c.refresh(), false); assert.equal(f.timers.size, 0); assert.equal(f.writes().length, 1);
});

async function phoneFixture(answer = job()) {
  const tracked = fixture({ jobs: [] }), calls = [];
  const c = A.model({ jobs: tracked.c, call: async (name, args) => {
    calls.push([name, args]); if (name === 'zones') return groups();
    if (answer instanceof Error) throw answer; return copy(answer);
  } });
  await c.refresh(); c.select('Main@S2', true); c.field('words', 'Dinner'); c.review();
  return { c, tracked, calls };
}

test('phone creation admits once and releases the form while the job remains active', async () => {
  const f = await phoneFixture(), reviewed = f.c.state.pending;
  assert.equal(await f.c.confirm(reviewed), true);
  assert.deepEqual(f.calls.filter(([name]) => name === 'announcementStart'), [['announcementStart', { body: { text: 'Dinner', rooms: ['Main@S2'], volume: 35 } }]]);
  assert.equal(f.c.state.writing, false); assert.equal(f.c.state.report, null); assert.equal(f.tracked.c.state.jobs[0].phase, 'queued');
  assert.match(f.c.state.message, /admitted/); assert.equal(await f.c.confirm(reviewed), false); f.c.dispose();
});

test('phone lost or mismatched admission replies never retry the creation command', async () => {
  for (const reply of [new Error('Lost reply'), job('queued', { rooms: ['Main@S1'] }), { done: 'No job id' }]) {
    const f = await phoneFixture(reply), reviewed = f.c.state.pending;
    assert.equal(await f.c.confirm(reviewed), false); assert.equal(await f.c.confirm(reviewed), false);
    assert.equal(f.calls.filter(([name]) => name === 'announcementStart').length, 1);
    assert.equal(f.tracked.c.state.jobs.length, 0); assert.match(f.c.state.message, /not repeated/); f.c.dispose();
  }
});

function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, children: [], open: false, hidden: false, disabled: false, checked: false, textContent: '', value: '', attrs: {},
      setAttribute(name, value) { this.attrs[name] = value; }, getAttribute(name) { return this.attrs[name]; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, fn) { listeners.set(name, fn); }, fire(name, extra = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...extra }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; },
      querySelector(query) { return find(this, node => query === `[data-announcement-job="${node.attrs?.['data-announcement-job']}"]`); },
    };
  }, getElementById(id) { if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id }); return nodes.get(id); } };
  return doc;
}
function find(node, predicate) { if (predicate(node)) return node; for (const child of node.children || []) { const found = find(child, predicate); if (found) return found; } return null; }
const content = node => [node.textContent || '', ...(node.children || []).map(content)].join(' ');

async function dashboardFixture({ panel = false, response = job() } = {}) {
  const calls = [], inventory = groups(), doc = documentFixture(); let announcer;
  const call = async (url, body) => {
    calls.push({ url, body });
    if (url === '/zones') return copy(inventory);
    if (url.startsWith('/zones/') && url.endsWith('/state')) {
      const target = decodeURIComponent(url.slice(7, -6)), zone = inventory.find(zone => `${zone.coordinator_room}@${zone.household}` === target);
      return { zone: copy(zone), transport_state: zone.transport_state, volume: 25, mute: false, track: null };
    }
    if (url === '/announcements') return body ? copy(response) : [copy(job('playing'))];
    if (url === `/announcements/${ID}/cancel`) return job('failed', { cancel_requested: true });
    throw new Error(`Unexpected request ${url}`);
  };
  const ctl = C.model({ call, now: () => 1000, changed: () => announcer?.observe() });
  const tracked = panel ? null : fixture({ jobs: [] });
  const options = { call, controller: ctl, now: () => 1000 };
  announcer = panel ? D.panel({ ...options, document: doc }) : D.model({ ...options, jobs: tracked.c });
  assert.equal(await ctl.refresh(), true); ctl.connection(true);
  await announcer.open(); await settle();
  return { ctl, announcer, calls, inventory, doc };
}

test('dashboard admits one exact reviewed job and releases its mutation lane before playback finishes', async () => {
  const f = await dashboardFixture(); f.announcer.select('Main@S2', true); f.announcer.edit('words', 'Dinner');
  const review = f.announcer.review(); assert.ok(review); assert.equal(await f.announcer.apply(review), true);
  assert.deepEqual(f.calls.filter(call => call.body !== undefined), [{ url: '/announcements', body: { rooms: ['Main@S2'], volume: 35, text: 'Dinner' } }]);
  assert.equal(f.ctl.state.busy, false); assert.equal(f.ctl.state.uncertain, false);
  assert.equal(f.announcer.jobs.state.jobs[0].phase, 'queued'); assert.equal(f.announcer.state.report, null);
  assert.equal(await f.announcer.apply(review), false); f.announcer.dispose();
});

test('dashboard rejects a wrong-household admission receipt and preserves an uncertain single dispatch', async () => {
  const f = await dashboardFixture({ response: job('queued', { rooms: ['Main@S1'] }) });
  f.announcer.select('Main@S2', true); f.announcer.edit('words', 'Dinner'); const review = f.announcer.review();
  assert.equal(await f.announcer.apply(review), false); assert.equal(f.ctl.state.uncertain, true);
  assert.equal(f.calls.filter(call => call.body !== undefined).length, 1); assert.equal(await f.announcer.apply(review), false); f.announcer.dispose();
});

test('production dashboard panel renders jobs and cancels by job ID independently of selected room', async t => {
  const f = await dashboardFixture({ panel: true }); t.after(() => f.announcer.dispose());
  const dialog = f.doc.getElementById('announcements-dialog'); assert.match(content(dialog), /Playing · speech/);
  const cancel = find(dialog, node => node.attrs?.['data-announcement-job'] === ID); assert.ok(cancel); assert.equal(cancel.disabled, false);
  f.ctl.select('Main@S1'); cancel.fire('click'); await settle();
  assert.deepEqual(f.calls.filter(call => call.body !== undefined), [{ url: `/announcements/${ID}/cancel`, body: {} }]);
  assert.match(content(dialog), /Needs attention/); assert.match(content(dialog), /Not restored: Manual source retained/);
  assert.equal(find(dialog, node => node.attrs?.['data-announcement-job'] === ID), null);
  f.announcer.close(); assert.equal(f.announcer.jobs.state.visible, false);
});

test('the real HTTP adapter sends one admission and one exact cancellation with observable restoration', async t => {
  const received = []; let current = null;
  const server = http.createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : undefined;
    received.push({ method: request.method, url: request.url, body });
    let value;
    if (request.url === '/announcements' && request.method === 'POST') { current = job(); value = current; }
    else if (request.url === `/announcements/${ID}/cancel` && request.method === 'POST') { current = job('cancelled'); value = current; }
    else if (request.url === '/announcements') value = current ? [current] : [];
    else if (request.url === '/zones') value = groups();
    else { response.writeHead(404); response.end('{}'); return; }
    response.writeHead(200, { 'Content-Type': 'application/json' }); response.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`, raw = C.request({ fetcher: (url, options) => fetch(base + url, options) });
  const call = (name, { params, body, signal } = {}) => {
    const [method, template] = name === 'zones' ? ['GET', '/zones'] : A.routes[name];
    const path = template.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    return raw(path, method === 'GET' ? undefined : body || {}, { signal });
  };
  const jobs = A.jobModel({ call }), phone = A.model({ call, jobs }); t.after(() => phone.dispose());
  await phone.refresh(); phone.select('Main@S2', true); phone.field('words', 'Dinner'); phone.review();
  assert.equal(await phone.confirm(), true); assert.equal(await jobs.cancel(jobs.state.jobs[0]), true);
  assert.equal(jobs.state.jobs[0].phase, 'cancelled'); assert.equal(jobs.state.jobs[0].result.households[0].outcome, 'cancelled');
  assert.deepEqual(received.filter(request => request.method === 'POST'), [
    { method: 'POST', url: '/announcements', body: { rooms: ['Main@S2'], volume: 35, text: 'Dinner' } },
    { method: 'POST', url: `/announcements/${ID}/cancel`, body: {} },
  ]);
});
