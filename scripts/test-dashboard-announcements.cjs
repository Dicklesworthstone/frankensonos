'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Announcements = require('../crates/fsonos-api/src/remote/dashboard-announcements.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 200 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, children: [], open: false, hidden: false, disabled: false, checked: false, textContent: '', value: '',
      attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; } };
  }, getElementById(id) {
    if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id });
    return nodes.get(id);
  } };
  return doc;
}
function wav() {
  const b = Buffer.alloc(48); b.write('RIFF'); b.writeUInt32LE(40, 4); b.write('WAVEfmt ', 8);
  b.writeUInt32LE(16, 16); b.writeUInt16LE(1, 20); b.writeUInt16LE(1, 22); b.writeUInt32LE(8000, 24);
  b.writeUInt32LE(16000, 28); b.writeUInt16LE(2, 32); b.writeUInt16LE(16, 34); b.write('data', 36); b.writeUInt32LE(4, 40);
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
}
function fixture({ demo = false, dom = false, network, readFile } = {}) {
  const zones = ['S2', 'S1'].map((household, i) => ({ household, coordinator_room: 'Main room',
    members: i ? ['Main room'] : ['Main room', 'Side room'], transport_state: i ? 'paused' : 'playing' }));
  const rows = new Map(zones.map(zone => [C.key(zone), { zone, transport_state: zone.transport_state, volume: 25, mute: false,
    track: { uri: 'fixture:track', title: 'Synthetic music', queue_position: 2, position_secs: 30, duration_secs: 180 } }]));
  const answer = { done: 'Playback and grouping restored', clean: true, households: [
    { lead: 'Side room', rooms: ['Side room'], levels: [{ room: 'Side room', volume: 25 }], outcome: 'finished' },
    { lead: 'Main room', rooms: ['Main room'], levels: [{ room: 'Main room', volume: 30 }], outcome: 'finished' },
  ] };
  let time = 1000, reader = null, writer = null, announcer;
  const calls = [], normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url.startsWith('/zones/') && url.endsWith('/state')) return copy(rows.get(decodeURIComponent(url.slice(7, -6))));
    throw new Error('Unexpected fixture read: ' + url);
  };
  const normalWrite = (url, body) => { assert.equal(url, '/announce'); assert.ok(body.rooms.length); return copy(answer); };
  const raw = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, options, normalWrite) : normalWrite(url, body);
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const call = network ? network(raw) : raw;
  const ctl = C.model({ call, demo, now: () => time, changed: () => announcer?.observe() });
  const options = { call, controller: ctl, now: () => time, readFile: readFile || (() => ({ promise: Promise.resolve(wav()), cancel() {} })) };
  const doc = dom ? documentFixture() : null;
  // These fixtures exercise the retained synchronous embedding contract. The
  // production job-backed panels have separate lifecycle/one-shot coverage.
  announcer = dom ? Announcements.panel({ ...options, document: doc, trackJobs: false }) : Announcements.model(options);
  return { ctl, announcer, zones, rows, answer, calls, doc, call,
    writes: () => calls.filter(row => row.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}
async function review(f) {
  assert.equal(await f.announcer.open(), true); f.announcer.select('Side room@S2', true); f.announcer.select('Main room@S1', true);
  f.announcer.edit('words', '  Dinner is ready  '); const p = f.announcer.review(); assert.ok(p); return p;
}

test('announcement inventory is lazy and requires explicit household-qualified physical rooms', async () => {
  const f = await fixture().ready(), before = f.calls.length;
  assert.equal(f.announcer.state.rooms.length, 0); assert.equal(await f.announcer.open(), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones']);
  assert.equal(f.announcer.state.selected.size, 0); f.announcer.edit('words', 'Hello');
  assert.equal(f.announcer.review(), null); assert.match(f.announcer.state.message, /Select at least one/);
  assert.equal(f.announcer.select('Main room', true), false); assert.equal(f.announcer.select('all', true), false);
  assert.equal(f.announcer.select('Main room@S1', true), true);
  assert.deepEqual(f.announcer.review().body.rooms, ['Main room@S1']); assert.equal(f.writes().length, 0);
});

test('speech sends immutable reviewed rooms, text, voice and volume through one native command', async () => {
  const f = await fixture().ready(); await review(f);
  f.announcer.edit('voice', ' en-US '); f.announcer.edit('volume', '0'); const p = f.announcer.review();
  for (const value of [p, p.body, p.body.rooms, p.rooms, ...p.rooms]) assert.ok(Object.isFrozen(value));
  assert.equal(await f.announcer.apply(p), true);
  assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/announce', body: {
    rooms: ['Side room@S2', 'Main room@S1'], volume: 0, text: 'Dinner is ready', voice: 'en-US',
  } }]);
  assert.equal(f.announcer.state.report.clean, true); assert.equal(f.announcer.state.report.households[0].levels[0].volume, 25);
  assert.equal(f.ctl.state.uncertain, false); assert.equal(await f.announcer.apply(p), false);
  assert.equal(f.announcer.canReview(), false, 'a new announcement needs another explicit room read');
  assert.equal(f.calls.slice(-3).filter(row => row.url.endsWith('/state')).length, 2, 'refreshes playback across the house');
});

test('all chimes exclude speech and voice fields', async () => {
  for (const chime of ['bell', 'beep', 'rise']) {
    const f = await fixture().ready(); await review(f); f.announcer.edit('voice', 'unused');
    f.announcer.edit('source', 'chime'); f.announcer.edit('chime', chime); f.announcer.review();
    assert.equal(await f.announcer.apply(), true);
    assert.deepEqual(f.writes()[0].body, { rooms: ['Side room@S2', 'Main room@S1'], volume: 35, chime });
  }
});

test('invalid speech, volume, voice and sources never dispatch or silently become defaults', async () => {
  const f = await fixture().ready(); await review(f);
  for (const [field, bad, good] of [['words', '', 'Hello'], ['words', '\ud800', 'Hello'], ['words', 'a'.repeat(1001), 'Hello'],
    ['voice', 'a\nb', ''], ['voice', 'a'.repeat(257), ''], ['volume', '101', '35'], ['volume', '1e2', '35'], ['volume', '-1', '35']]) {
    f.announcer.edit(field, bad); assert.equal(f.announcer.canReview(), false); assert.equal(f.announcer.review(), null);
    assert.equal(await f.announcer.apply(), false); f.announcer.edit(field, good);
  }
  assert.equal(f.announcer.edit('source', 'url'), false); assert.equal(f.announcer.edit('rooms', 'all'), false);
  f.announcer.edit('words', '🎵'.repeat(1000)); assert.ok(f.announcer.review()); assert.equal(f.writes().length, 0);
});

test('editing a review, cancelling and refreshing invalidate old confirmations', async () => {
  const f = await fixture().ready(), first = await review(f);
  f.announcer.edit('volume', '20'); assert.equal(await f.announcer.apply(first), false);
  const second = f.announcer.review(); f.announcer.select('Side room@S2', false); assert.equal(await f.announcer.apply(second), false);
  const third = f.announcer.review(); f.announcer.cancelReview(); assert.equal(await f.announcer.apply(third), false);
  const fourth = f.announcer.review(); await f.announcer.read(); assert.equal(await f.announcer.apply(fourth), false);
  assert.equal(f.writes().length, 0);
});

test('changed membership, coordinator or household rejects preflight without sending', async () => {
  for (const change of [f => f.zones[0].members.push('New room'), f => { f.zones[0].coordinator_room = 'Side room'; },
    f => { f.zones[1].household = 'Other'; }]) {
    const f = await fixture().ready(); await review(f); change(f);
    assert.equal(await f.announcer.apply(), false); assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false); assert.match(f.announcer.state.message, /grouping changed/);
  }
});

test('ordering and playback progress are not group identity changes', async () => {
  const f = await fixture().ready(); await review(f); f.zones.reverse(); f.zones[1].members.reverse();
  f.zones[1].transport_state = 'paused'; f.rows.get('Main room@S2').track.position_secs++;
  assert.equal(await f.announcer.apply(), true); assert.equal(f.writes().length, 1);
});

test('expiry, a stale selected group and topology invalidation block confirmation', async () => {
  for (const change of [f => f.time(1000 + Announcements.MAX_AGE), f => f.ctl.invalidate(['Main room@S2']), f => f.ctl.invalidate()]) {
    const f = await fixture().ready(), p = await review(f); change(f); f.announcer.observe();
    assert.equal(f.announcer.state.pending, null); assert.equal(await f.announcer.apply(p), false); assert.equal(f.writes().length, 0);
  }
});

test('a delayed preflight cannot outlive its reviewed observation', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred();
  f.read(url => { assert.equal(url, '/zones'); return gate.promise; });
  const work = f.announcer.apply(); await settled(() => f.ctl.state.busy);
  f.time(1000 + Announcements.MAX_AGE); gate.resolve(copy(f.zones));
  assert.equal(await work, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
});

test('the shared lane excludes playback, stored routines and double confirmation during preflight', async () => {
  const f = await fixture().ready(), p = await review(f), gate = deferred();
  f.read((url, options, normal) => url === '/zones' ? gate.promise : normal(url));
  const work = f.announcer.apply(p); await settled(() => f.ctl.state.busy);
  assert.equal(await f.announcer.apply(p), false); assert.equal(await f.ctl.command('pause'), false);
  assert.equal(await f.ctl.mutate(() => assert.fail('stored operation must not begin'), { stored: 'schedules' }), false);
  assert.equal(f.announcer.edit('words', 'Changed'), false); assert.equal(await f.announcer.read(), false);
  gate.resolve(copy(f.zones)); assert.equal(await work, true); assert.equal(f.writes().length, 1);
});

test('another operation cannot be bypassed by an announcement review', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred();
  const other = f.ctl.mutate(async () => { await gate.promise; return { done: 'Stored', changed: true }; }, { stored: 'schedules' });
  assert.equal(await f.announcer.apply(), false); gate.resolve(); await other; assert.equal(f.writes().length, 0);
});

test('close, suspend and dispose during preflight fence every unsent write', async () => {
  for (const action of ['close', 'suspend', 'dispose']) {
    const f = await fixture().ready(); await review(f); const gate = deferred();
    f.read(() => gate.promise); const work = f.announcer.apply(); await settled(() => f.ctl.state.busy);
    const signal = f.calls.at(-1).options.signal;
    f.announcer[action](); assert.equal(signal.aborted, true); gate.resolve(copy(f.zones));
    assert.equal(await work, false); assert.equal(f.writes().length, 0);
  }
});

test('close or suspend after dispatch never aborts or repeats audio and preserves a late receipt', async () => {
  for (const action of ['close', 'suspend']) {
    const f = await fixture().ready(); await review(f); const gate = deferred(); f.write(() => gate.promise);
    const work = f.announcer.apply(); await settled(() => f.writes().length === 1);
    assert.equal(f.writes()[0].options?.signal, undefined); f.announcer[action]();
    gate.resolve(copy(f.answer)); assert.equal(await work, true);
    assert.equal(f.announcer.state.open, false); assert.equal(f.announcer.state.report.clean, true);
    f.announcer.resume(); await f.announcer.open(); assert.equal(f.announcer.state.report.clean, true);
    assert.equal(f.writes().length, 1); assert.equal(f.announcer.state.selected.size, 0);
  }
});

test('lost and malformed responses share Core uncertainty and routine refresh cannot clear it', async () => {
  for (const answer of [new Error('Connection ended'), {}, { done: 'ok', clean: true, households: [{ outcome: 'failed' }] }]) {
    const f = await fixture().ready(); const p = await review(f);
    f.write(() => { if (answer instanceof Error) throw answer; return copy(answer); });
    assert.equal(await f.announcer.apply(), false); assert.equal(f.ctl.state.uncertain, true);
    assert.match(f.announcer.state.message, /Outcome uncertain; no retry/); assert.equal(await f.announcer.apply(p), false);
    assert.equal(await f.ctl.command('pause'), false); assert.equal(f.writes().length, 1);
    f.read((url, options, normal) => url === '/schedules' ? [] : normal(url));
    await f.ctl.refreshStored('schedules', value => value, { explicit: true }); assert.equal(f.ctl.state.uncertain, true);
    await f.ctl.refresh(true); assert.equal(f.ctl.state.uncertain, false);
    assert.equal(f.announcer.canReview(), false, 'room observation still needs refresh');
  }
});

test('incomplete, duplicate and unexpected room receipts cannot claim clean restoration', async () => {
  for (const change of [answer => answer.households.pop(), answer => answer.households[1].rooms.push('Side room'),
    answer => { answer.households[1].rooms[0] = 'Unknown room'; }]) {
    const f = await fixture().ready(); await review(f); change(f.answer);
    assert.equal(await f.announcer.apply(), false); assert.equal(f.ctl.state.uncertain, true);
    assert.match(f.announcer.state.message, /did not account for every reviewed room/); assert.equal(f.writes().length, 1);
    assert.equal(f.announcer.state.report, null);
  }
});

test('receipt coverage preserves repeated physical names across households without inventing identity', async () => {
  const f = await fixture().ready(); await review(f);
  f.announcer.select('Side room@S2', false); f.announcer.select('Main room@S2', true); f.announcer.review();
  f.answer.households[0].lead = 'Main room'; f.answer.households[0].rooms = ['Main room'];
  f.answer.households[0].levels = [{ room: 'Main room', volume: 25 }]; f.answer.households.reverse();
  assert.equal(await f.announcer.apply(), true); assert.equal(f.announcer.state.report.households.length, 2);
  assert.equal(f.ctl.state.uncertain, false);
});

test('page suspension after send preserves the receipt without starting background playback refresh', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); f.write(() => gate.promise);
  const work = f.announcer.apply(); await settled(() => f.writes().length === 1);
  f.announcer.suspend(); f.ctl.suspend(); const before = f.calls.length;
  gate.resolve(copy(f.answer)); assert.equal(await work, true);
  assert.equal(f.announcer.state.report.clean, true); assert.equal(f.calls.length, before);
  assert.equal(f.announcer.state.open, false); assert.equal(f.ctl.state.active, false);
});

test('partial native reports retain actual levels and restoration failures without global uncertainty', async () => {
  const f = await fixture().ready(); await review(f); f.answer.clean = false;
  Object.assign(f.answer.households[1], { outcome: 'deadline', error: 'Playback deadline', not_restored: ['Main room: grouping could not be restored'] });
  assert.equal(await f.announcer.apply(), true); assert.equal(f.ctl.state.uncertain, false);
  assert.equal(f.announcer.state.report.clean, false); assert.equal(f.announcer.state.report.households[0].outcome, 'finished');
  assert.deepEqual(f.announcer.state.report.households[1].not_restored, ['Main room: grouping could not be restored']);
  assert.match(f.announcer.state.message, /did not complete cleanly/);
});

test('failed playback refresh cannot turn a received announcement result into a retry', async () => {
  const f = await fixture().ready(); await review(f);
  f.write((url, body, options, normal) => { f.read(() => { throw new Error('Speaker offline after restore'); }); return normal(url, body); });
  assert.equal(await f.announcer.apply(), true); assert.equal(f.ctl.state.uncertain, false);
  assert.equal(f.announcer.state.report.clean, true); assert.equal(f.writes().length, 1);
});

test('demo mode and offline playback cannot dispatch announcements', async () => {
  const demo = await fixture({ demo: true }).ready(); await demo.announcer.open(); demo.announcer.select('Side room@S2', true);
  demo.announcer.edit('words', 'Hello'); assert.equal(demo.announcer.review(), null); assert.equal(demo.writes().length, 0);
  const offline = await fixture().ready(); await review(offline); offline.ctl.connection(false);
  assert.equal(await offline.announcer.apply(), false); assert.equal(offline.writes().length, 0);
});

test('late room reads cannot reopen the dialog or overwrite a newer inventory', async () => {
  const f = await fixture().ready(), gate = deferred(); f.read(() => gate.promise);
  const work = f.announcer.open(); await settled(() => f.announcer.state.loading); const signal = f.calls.at(-1).options.signal;
  f.announcer.close(); assert.equal(signal.aborted, true); f.read(null); await f.announcer.open();
  gate.resolve([]); assert.equal(await work, false); assert.equal(f.announcer.state.rooms.length, 3); assert.equal(f.announcer.state.loading, false);
});

test('WAV bytes stay local through review and upload once without the filename', async () => {
  const f = await fixture().ready(); await review(f); f.announcer.edit('source', 'wav');
  assert.equal(await f.announcer.chooseWav({ size: 48, name: 'doorbell.wav' }), true);
  const p = f.announcer.review(); assert.match(p.label, /doorbell.wav/); assert.equal(f.writes().length, 0);
  assert.equal(await f.announcer.apply(), true); const body = f.writes()[0].body;
  assert.equal(body.wav_base64, Buffer.from(wav()).toString('base64')); assert.equal(body.text, undefined);
  assert.equal(JSON.stringify(body).includes('doorbell.wav'), false); assert.equal(f.announcer.state.wav, null);
});

test('invalid, oversized and changed-size files cannot reuse an older reviewed clip', async () => {
  let bytes = wav(), count = 0;
  const f = await fixture({ readFile() { count++; return { promise: Promise.resolve(bytes), cancel() {} }; } }).ready();
  await review(f); f.announcer.edit('source', 'wav'); await f.announcer.chooseWav({ size: 48, name: 'good.wav' });
  const p = f.announcer.review(); assert.equal(await f.announcer.chooseWav({ size: 16 * 1024 * 1024 + 1 }), false);
  assert.equal(count, 1); assert.equal(f.announcer.state.wav, null); assert.equal(await f.announcer.apply(p), false);
  bytes = new ArrayBuffer(48); assert.equal(await f.announcer.chooseWav({ size: 48 }), false);
  bytes = wav(); assert.equal(await f.announcer.chooseWav({ size: 49 }), false); assert.equal(f.writes().length, 0);
});

test('source changes, file replacement and closing cancel reads and fence late completions', async () => {
  const reads = [];
  const f = await fixture({ readFile() { const gate = deferred(); const work = { ...gate, cancelled: false, cancel() { this.cancelled = true; } }; reads.push(work); return work; } }).ready();
  await review(f); f.announcer.edit('source', 'wav');
  const first = f.announcer.chooseWav({ size: 48 }); const second = f.announcer.chooseWav({ size: 48 });
  assert.equal(reads[0].cancelled, true); reads[0].resolve(wav()); assert.equal(await first, false); assert.equal(f.announcer.state.fileLoading, true);
  f.announcer.edit('source', 'chime'); assert.equal(reads[1].cancelled, true); reads[1].resolve(wav()); assert.equal(await second, false);
  f.announcer.edit('source', 'wav'); const third = f.announcer.chooseWav({ size: 48 }); f.announcer.close();
  assert.equal(reads[2].cancelled, true); reads[2].resolve(wav()); assert.equal(await third, false); assert.equal(f.announcer.state.wav, null);
});

test('panel handlers review groups, restore focus and expose partial results as text', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-announcements').focus(); $('open-announcements').fire('click'); await settled(() => !f.announcer.state.loading);
  assert.equal($('announcements-dialog').open, true); assert.equal($('announcement-rooms').children.length, 3);
  const checkbox = $('announcement-rooms').children[1].children[0]; checkbox.checked = true; checkbox.fire('change');
  const second = $('announcement-rooms').children[2].children[0]; second.checked = true; second.fire('change');
  $('announcement-words').value = '<script>synthetic text</script>'; $('announcement-words').fire('input');
  $('announcement-review').focus(); $('announcement-review').fire('click');
  assert.equal(f.doc.activeElement.id, 'announcement-preview'); assert.equal($('announcement-preview-source').textContent, '<script>synthetic text</script>');
  assert.match($('announcement-preview-groups').textContent, /Main room \+ Side room/);
  $('announcement-back').fire('click'); assert.equal(f.doc.activeElement.id, 'announcement-review');
  $('announcement-review').fire('click'); f.answer.clean = false; f.answer.households[0].not_restored = ['<unsafe>: volume'];
  $('announcement-confirm').fire('click'); await settled(() => !!f.announcer.state.report);
  assert.equal($('announcement-result').hidden, false); assert.equal($('announcement-result-title').textContent, 'Check announcement results');
  const content = node => [node.textContent, ...node.children.map(content)].join(' ');
  assert.match(content($('announcement-result-detail')), /Not restored: <unsafe>: volume/);
  assert.match(content($('announcement-result-detail')), /Side room: 25/);
  $('close-announcements').fire('click'); assert.equal(f.doc.activeElement.id, 'open-announcements'); assert.equal($('announcements-dialog').open, false);
});

test('shared full-remote helpers load without remote globals or automatic mounting', () => {
  const context = vm.createContext({ console });
  vm.runInContext(fs.readFileSync(require.resolve('../crates/fsonos-api/src/remote/announcements.js'), 'utf8'), context);
  assert.equal(vm.runInContext('typeof AnnouncementRemote.proposal', context), 'function');
  assert.equal(vm.runInContext('typeof AnnouncementRemote.readLocalFile', context), 'function');
});

test('real request adapter sends one JSON announcement through a loopback server', async t => {
  let raw, origin; const received = [];
  const server = http.createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : undefined;
    received.push({ method: request.method, url: request.url, body });
    try { const value = await raw(request.url, body); response.writeHead(200, { 'Content-Type': 'application/json' }); response.end(JSON.stringify(value)); }
    catch (error) { response.writeHead(500, { 'Content-Type': 'application/json' }); response.end(JSON.stringify({ message: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve)); origin = 'http://127.0.0.1:' + server.address().port;
  t.after(() => new Promise(resolve => server.close(resolve)));
  const f = await fixture({ network(fn) { raw = fn; return C.request({ fetcher: (url, options) => fetch(origin + url, options) }); } }).ready();
  await review(f); assert.equal(await f.announcer.apply(), true);
  const writes = received.filter(row => row.method !== 'GET'); assert.equal(writes.length, 1);
  assert.deepEqual(writes[0], { method: 'POST', url: '/announce', body: { rooms: ['Side room@S2', 'Main room@S1'], volume: 35, text: 'Dinner is ready' } });
  assert.equal(f.announcer.state.report.clean, true);
});
