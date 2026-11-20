'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const EQ = require('../crates/fsonos-api/src/remote/dashboard-equalizer.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
async function settled(predicate) {
  for (let n = 0; n < 500 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached its expected boundary');
}
function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, children: [], open: false, hidden: false, disabled: false, textContent: '', value: '', checked: false,
      attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; } };
  }, getElementById(id) {
    if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id });
    return nodes.get(id);
  } };
  return doc;
}
function fixture({ demo = false, dom = false, count = 2 } = {}) {
  const names = ['Main room', ...Array.from({ length: count - 1 }, (_, i) => i === 0 ? 'Side room' : 'Room ' + (i + 1))];
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Main room', members: [...names], transport_state: 'playing' }));
  const tones = new Map(zones.flatMap(zone => zone.members.map(name => [name + '@' + zone.household,
    { room: name, bass: 1, treble: -2, loudness: true }])));
  let time = 1000, reader = null, writer = null, sound;
  const calls = [], doc = dom ? documentFixture() : null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url.endsWith('/state')) {
      const target = decodeURIComponent(url.slice(7, -6));
      const zone = zones.find(z => z.members.some(name => name + '@' + z.household === target));
      return { zone: copy(zone), transport_state: 'playing', volume: 20, mute: false, track: { uri: 'fixture:track', queue_position: 1 } };
    }
    if (url.endsWith('/eq')) {
      const target = decodeURIComponent(url.slice(7, -3)); assert.ok(tones.has(target)); return copy(tones.get(target));
    }
    throw new Error('Unexpected read: ' + url);
  };
  const normalWrite = (url, body) => {
    assert.equal(url, '/eq'); const eq = tones.get(body.zone); assert.ok(eq);
    for (const field of ['bass', 'treble', 'loudness']) if (field in body) eq[field] = body[field];
    return { done: eq.room + ' sound changed', eq: copy(eq) };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, normalWrite) : normalWrite(url, body);
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => sound?.observe() });
  sound = dom ? EQ.panel({ call, controller: ctl, now: () => time, document: doc }) : EQ.model({ call, controller: ctl, now: () => time });
  return { ctl, sound, zones, tones, calls, doc, normalRead, normalWrite,
    writes: () => calls.filter(c => c.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(n) { time = n; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}
async function review(f, field = 'bass', value = 4, target = 'Side room@S2') {
  await f.sound.open(); assert.equal(f.sound.choose(target, field, value), true);
  const pending = f.sound.review(target); assert.ok(pending); return pending;
}

test('tone reads are lazy, household qualified, and include noncoordinator rooms', async () => {
  const f = await fixture().ready(); assert.equal(f.calls.some(c => c.url.endsWith('/eq')), false);
  const before = f.calls.length; assert.equal(await f.sound.open(), true);
  assert.deepEqual(f.calls.slice(before).map(c => c.url), ['/zones', '/zones/Main%20room%40S2/eq', '/zones/Side%20room%40S2/eq', '/zones']);
  assert.deepEqual(f.sound.state.rows.map(r => [r.target, r.bass, r.treble, r.loudness]),
    [['Main room@S2', 1, -2, true], ['Side room@S2', 1, -2, true]]);
  assert.equal(f.writes().length, 0);
});

test('sound reads use at most four concurrent requests', async () => {
  const f = await fixture({ count: 9 }).ready(), gates = [];
  let active = 0, peak = 0;
  f.read(async (url, options, normal) => {
    if (!url.endsWith('/eq')) return normal(url);
    const gate = deferred(); gates.push(gate); peak = Math.max(peak, ++active); await gate.promise; --active; return normal(url);
  });
  const opening = f.sound.open();
  for (const [start, end] of [[0, 4], [4, 8], [8, 9]]) {
    await settled(() => gates.length === end); for (const gate of gates.slice(start, end)) gate.resolve();
  }
  assert.equal(await opening, true); assert.equal(peak, 4); assert.equal(f.sound.state.rows.length, 9);
});

test('one edited field reaches exactly the reviewed member and reads back the accepted sound', async () => {
  const f = await fixture().ready(), pending = await review(f);
  assert.ok(Object.isFrozen(pending) && Object.isFrozen(pending.body) && Object.isFrozen(pending.wanted));
  assert.equal(await f.sound.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/eq', body: { zone: 'Side room@S2', bass: 4 } }]);
  assert.equal(f.tones.get('Main room@S2').bass, 1); assert.equal(f.tones.get('Side room@S1').bass, 1);
  assert.deepEqual(f.sound.desired('Side room@S2'), { bass: 4, treble: -2, loudness: true });
  assert.equal(f.sound.state.result.target, 'Side room@S2'); assert.equal(f.ctl.state.uncertain, false);
  assert.equal(f.sound.canReview('Side room@S2'), false);
  assert.equal(await f.sound.apply(pending), false); assert.equal(f.writes().length, 1);
});

test('all three settings, flat sound, discard and boundaries remain local until confirmed', async () => {
  const f = await fixture().ready(); await f.sound.open(); const target = 'Side room@S2';
  assert.equal(f.sound.choose(target, 'bass', -10), true);
  assert.equal(f.sound.choose(target, 'treble', 10), true);
  assert.equal(f.sound.choose(target, 'loudness', false), true);
  assert.deepEqual(f.sound.review(target).body, { zone: target, bass: -10, treble: 10, loudness: false });
  assert.equal(f.writes().length, 0); f.sound.reset(target);
  assert.deepEqual(f.sound.desired(target), { bass: 1, treble: -2, loudness: true }); assert.equal(f.sound.state.pending, null);
  f.sound.reset(target, true); assert.deepEqual(f.sound.desired(target), { bass: 0, treble: 0, loudness: false });
  for (const [field, value] of [['bass', 11], ['bass', -11], ['treble', 0.5], ['bass', '2'], ['loudness', 1], ['volume', 20]]) {
    assert.equal(f.sound.choose(target, field, value), false);
  }
  assert.ok(f.sound.review(target)); assert.equal(await f.sound.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/eq', body: { zone: target, bass: 0, treble: 0, loudness: false } }]);
});

test('failed and malformed room reads retain healthy rooms without inventing flat settings', async () => {
  const f = await fixture().ready(); f.read((url, options, normal) => {
    if (url.includes('Side') && url.endsWith('/eq')) return { room: 'Side room', bass: 99, treble: 0, loudness: true };
    return normal(url);
  });
  assert.equal(await f.sound.open(), true); assert.equal(f.sound.desired('Side room@S2'), null);
  assert.equal(f.sound.choose('Side room@S2', 'bass', 0), false);
  assert.equal(f.sound.choose('Main room@S2', 'bass', 2), true);
  assert.match(f.sound.state.rows[1].error, /incomplete/); assert.equal(f.writes().length, 0);
});

test('rereading all current fields prevents an edit from overwriting a different controller', async () => {
  const f = await fixture().ready(); await review(f); f.tones.get('Side room@S2').loudness = false;
  assert.equal(await f.sound.apply(), false); assert.equal(f.writes().length, 0);
  assert.match(f.sound.state.message, /speaker sound changed/); assert.equal(f.ctl.state.uncertain, false);
});

test('membership changes before or during tone preflight prevent the write', async () => {
  for (const during of [false, true]) {
    const f = await fixture().ready(); await review(f);
    if (during) f.read((url, options, normal) => {
      const answer = normal(url); if (url.endsWith('/eq')) f.zones[0].members = ['Main room']; return answer;
    });
    else f.zones[0].members = ['Main room'];
    assert.equal(await f.sound.apply(), false); assert.equal(f.writes().length, 0); assert.match(f.sound.state.message, /group or coordinator changed/);
  }
});

test('closed and suspended observations cannot publish or revive a review', async () => {
  for (const end of ['close', 'suspend', 'dispose']) {
    const f = await fixture().ready(), gate = deferred();
    f.read(async (url, options, normal) => { const answer = normal(url); if (url.endsWith('/eq')) await gate.promise; return answer; });
    const reading = f.sound.open(); await settled(() => f.calls.some(c => c.url.endsWith('/eq')));
    f.sound[end](); gate.resolve(); assert.equal(await reading, false);
    assert.equal(f.sound.state.rows, null); assert.equal(f.sound.state.pending, null); assert.equal(f.writes().length, 0);
  }
});

test('closing during preflight cancels a not-yet-sent change', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred();
  f.read(async (url, options, normal) => { const answer = normal(url); if (url.endsWith('/eq')) await gate.promise; return answer; });
  const change = f.sound.apply(); await settled(() => f.ctl.state.busy); f.sound.close(); gate.resolve();
  assert.equal(await change, false); assert.equal(f.writes().length, 0);
});

test('a submitted edit keeps its receipt across close and cannot dispatch twice', async () => {
  const f = await fixture().ready(), pending = await review(f), gate = deferred();
  f.write(async (url, body, normal) => { const result = normal(url, body); await gate.promise; return result; });
  const change = f.sound.apply(); await settled(() => f.writes().length === 1);
  assert.equal(await f.sound.apply(pending), false); assert.equal(await f.ctl.command('pause'), false);
  f.sound.close(); assert.equal(await f.sound.open(), false); gate.resolve();
  assert.equal(await change, true); assert.equal(f.sound.state.result.target, 'Side room@S2');
  assert.equal(f.sound.state.result.eq.bass, 4); assert.equal(f.writes().length, 1);
});

test('lost or malformed acknowledgements mark uncertainty without replay or invented success', async () => {
  for (const answer of [null, { done: 'Changed', eq: { room: 'Wrong room', bass: 4, treble: -2, loudness: true } },
    { done: 'Changed', eq: { room: 'Side room', bass: 3, treble: -2, loudness: true } },
    { eq: { room: 'Side room', bass: 4, treble: -2, loudness: true } }]) {
    const f = await fixture().ready(); await review(f);
    f.write((url, body, normal) => { normal(url, body); if (answer === null) throw new Error('Reply lost'); return answer; });
    assert.equal(await f.sound.apply(), false); assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.sound.state.result, null); assert.equal(f.sound.state.stale, true); assert.equal(f.writes().length, 1);
    assert.equal(f.sound.choose('Side room@S2', 'treble', 0), false);
    assert.equal(await f.sound.apply(), false); assert.equal(f.writes().length, 1);
  }
});

test('successful acknowledgement remains visible when subsequent sound reads fail', async () => {
  const f = await fixture().ready(); await review(f);
  f.write((url, body, normal) => {
    const result = normal(url, body); f.read((url, options, read) => { if (url.endsWith('/eq')) throw new Error('Speaker offline'); return read(url); }); return result;
  });
  assert.equal(await f.sound.apply(), true); assert.equal(f.sound.state.result.eq.bass, 4);
  assert.equal(f.ctl.state.uncertain, false); assert.match(f.sound.state.rows[1].error, /Speaker offline/);
  assert.equal(f.sound.desired('Side room@S2'), null); assert.equal(f.writes().length, 1);
});

test('old observations, changed selection and preview mode cannot authorize sound edits', async () => {
  const f = await fixture().ready(); const pending = await review(f); f.time(1000 + EQ.MAX_AGE);
  assert.equal(f.sound.canApply(), false); assert.equal(await f.sound.apply(), false);
  f.time(1000); f.ctl.select('Main room@S1'); assert.equal(f.sound.state.pending, null);
  assert.equal(await f.sound.apply(pending), false); assert.equal(f.writes().length, 0);
  const demo = await fixture({ demo: true }).ready(); await demo.sound.open();
  assert.equal(demo.sound.choose('Main room@S2', 'bass', 4), false); assert.equal(demo.writes().length, 0);
});

test('panel binds member sliders, local flat/reset, review, confirmation and focus restoration', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-speaker-sound').focus(); $('open-speaker-sound').fire('click'); await settled(() => !f.sound.state.loading);
  assert.equal($('speaker-sound-dialog').open, true);
  const row = $('speaker-sound-rooms').children[1], bass = row.children[2].children[1], loudness = row.children[4].children[0];
  assert.equal(bass.attributes['aria-label'], 'Side room@S2 bass'); bass.value = '5'; bass.fire('input');
  loudness.checked = false; loudness.fire('change'); assert.equal(f.writes().length, 0);
  const button = row.children[5].children[0]; button.focus(); button.fire('click');
  assert.equal(f.doc.activeElement.id, 'speaker-sound-preview'); assert.match($('speaker-sound-after').textContent, /bass \+5.*loudness off/);
  $('speaker-sound-confirm').fire('click'); await settled(() => !f.sound.state.writing && !f.sound.state.loading);
  assert.deepEqual(f.writes(), [{ url: '/eq', body: { zone: 'Side room@S2', bass: 5, loudness: false } }]);
  assert.match($('speaker-sound-result').textContent, /Side room@S2: bass \+5/);
  $('speaker-sound-dialog').fire('cancel'); assert.equal(f.sound.state.open, false); assert.equal(f.doc.activeElement.id, 'open-speaker-sound');
});

test('production assets expose speaker sound and connect every dashboard lifecycle', () => {
  const root = path.resolve(__dirname, '../crates/fsonos-api/src');
  const html = fs.readFileSync(path.join(root, 'remote/dashboard.html'), 'utf8');
  const script = fs.readFileSync(path.join(root, 'remote/dashboard.js'), 'utf8');
  const served = fs.readFileSync(path.join(root, 'http/remote/dashboard.rs'), 'utf8');
  assert.match(html, /id="open-speaker-sound"[^>]*aria-controls="speaker-sound-dialog"/);
  assert.match(html, /id="speaker-sound-dialog"[^>]*aria-labelledby="speaker-sound-title"/);
  assert.match(script, /speakerSound = DashboardEqualizer\.panel/);
  for (const action of ['observe', 'suspend', 'resume']) assert.match(script, new RegExp(`speakerSound\\?\\.${action}\\(\\)`));
  assert.match(served, /dashboard-equalizer\.js/); assert.match(served, /dashboard-equalizer\.css/);
  assert.match(served, /\{VOLUME\}\\n\{EQUALIZER\}\\n\{SCENES\}/);
});
