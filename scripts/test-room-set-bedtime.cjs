// Production room-set reviews, both browser mounts, and real HTTP delivery.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const Sets = require('../crates/fsonos-api/src/remote/room-sets.js');
const Panel = require('../crates/fsonos-api/src/remote/room-sets-panel.js');
const Sleep = require('../crates/fsonos-api/src/remote/dashboard-sleep.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const fixtures = require('./schedule-targets-fixture.cjs');
const root = path.join(__dirname, '..');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const tick = () => new Promise(resolve => setImmediate(resolve));
async function until(predicate) { for (let n = 0; n < 50 && !predicate(); n++) await tick(); assert.ok(predicate(), 'operation reached the expected boundary'); }
const error = (code = 'PLAYER_UNREACHABLE') => ({ code, detail: 'The original group could not complete this action.', hint: 'Read this group before another change.', retryable: false });
const timer = (zone, seconds = 1800, extra = {}) => ({ zone, done: 'Sleep timer set', changed: true,
  ends: '2030-01-01T01:00:00Z', remaining_secs: seconds, fades: true, fading: false, ...extra });

function fixture(options = {}) {
  const f = { zones: [
    { household: 'S1', coordinator_room: 'East', members: ['East', 'Side'], transport_state: 'playing' },
    { household: 'S2', coordinator_room: 'East', members: ['East'], transport_state: 'paused' },
  ], aliases: { bedtime: ['Side@S1', 'East@S2'], here: ['Side@S1'] }, physical: '', calls: [], custom: null, locks: [] };
  f.rows = new Map(f.zones.map((zone, i) => [C.key(zone), { zone: copy(zone), transport_state: i ? 'paused' : 'playing',
    transport_status: 'OK', volume: 20, mute: false, track: { uri: 'fixture:recording-' + i, title: 'Recording ' + i,
      queue_position: 2, position_secs: 20, duration_secs: 300 } }]));
  f.timers = new Map(f.zones.map((zone, i) => [C.key(zone), timer(zone.coordinator_room, 1800 + i * 900)]));
  f.resolve = target => fixtures.resolve(f.zones, target, f.aliases, f.physical);
  f.result = (name, body) => {
    const view = Sets.resolutionOf(f.resolve(body.zone), body.zone);
    if (name === 'setFadePause') return { done: 'Reviewed groups handled', changed: true,
      batch: { complete: true, results: view.groups.map(group => ({ household: group.household,
        target: group.coordinator, zone: group.coordinator_room,
        result: { status: 'succeeded', done: group.household === 'S2' ? 'Already paused; no fade needed' : 'Faded and paused',
          changed: group.household !== 'S2' } })) } };
    const applied = [], failed = [];
    for (const group of view.groups) {
      const address = `${group.coordinator_room}@${group.household}`, before = f.timers.get(address);
      if (body.extend && !before) {
        failed.push({ household: group.household, coordinator: group.coordinator, zone: group.coordinator_room,
          error: { ...error('INVALID_ARGUMENT'), detail: 'No active sleep timer to extend.' } }); continue;
      }
      const seconds = body.cancel ? null : Number(body.duration.slice(0, -1)) * 60 + (body.extend ? before.remaining_secs : 0);
      const value = timer(group.coordinator_room, seconds, body.cancel
        ? { ends: null, remaining_secs: null, changed: !!before, done: 'Sleep timer cancelled' }
        : { done: body.extend ? 'Sleep timer extended' : 'Sleep timer set' });
      applied.push({ household: group.household, coordinator: group.coordinator, timer: value });
    }
    if (view.groups.length === 1 && !failed.length) return applied[0].timer;
    return { done: failed.length ? 'Some sleep timers could not change' : 'Sleep timers changed', complete: !failed.length,
      changed: applied.some(row => row.timer.changed), applied, failed };
  };
  f.read = (name, args = {}) => {
    if (name === 'setRooms') return fixtures.rooms(f.zones, f.aliases);
    if (name === 'resolveRooms') return f.resolve(args.query.target);
    if (name === 'state') return copy(f.rows.get(args.params.room));
    if (name === 'setSleepRead') { const value = f.timers.get(args.query.zone); return value ? [copy(value)] : []; }
    throw new Error('Unexpected read ' + name);
  };
  f.call = async (name, args = {}) => {
    f.calls.push({ name, args });
    if (f.custom) { const value = await f.custom(name, args); if (value !== undefined) return value; }
    return ['setSleep', 'setFadePause'].includes(name) ? f.result(name, args.body) : f.read(name, args);
  };
  f.api = Sets.model({ call: f.call, execute: async (groups, work) => { f.locks.push(groups); await work(); }, ...options });
  f.writes = () => f.calls.filter(row => ['setSleep', 'setFadePause'].includes(row.name));
  f.review = async (action = 'sleep_set', target = 'bedtime') => {
    f.api.open(); f.api.edit('input', target); f.api.edit('action', action); f.api.edit('duration', '15m'); f.api.edit('seconds', '10');
    assert.equal(await f.api.review(), true, f.api.state.message); return f.api.state.preview;
  };
  f.urlCall = async (url, body, options = {}) => {
    if (url === '/zones') return copy(f.zones);
    const entries = { ...Sets.routes, state: ['GET', '/zones/{room}/state'] };
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname.endsWith('/state')) return f.call('state', { params: { room: decodeURIComponent(parsed.pathname.split('/')[2]) }, signal: options.signal });
    const route = Object.entries(entries).find(([, value]) => value[0] === (body === undefined ? 'GET' : 'POST') && value[1] === parsed.pathname);
    assert.ok(route, url); return f.call(route[0], body === undefined ? { query, signal: options.signal } : { body });
  };
  return f;
}

for (const action of ['sleep_set', 'sleep_extend', 'sleep_cancel', 'fade_pause']) {
  test(`${action} reviews every original group and sends one audience-bound command`, async () => {
    const f = fixture(), p = await f.review(action);
    assert.equal(p.view.groups.length, 2); assert.equal(p.sources.length, 2);
    assert.ok(p.view.groups.some(group => group.members.some(room => room.room === 'Side')));
    for (const source of p.sources) { assert.match(source.label, /Recording/); assert.match(source.label, /Timer pauses/); }
    assert.equal(f.writes().length, 0);
    assert.equal(await f.api.confirm(p), true, f.api.state.result?.detail);
    const expected = { zone: 'bedtime', expected_revision: p.view.revision,
      ...(action === 'fade_pause' ? { fade_secs: 10 } : action === 'sleep_cancel' ? { cancel: true }
        : { duration: '15m', ...(action === 'sleep_extend' ? { extend: true } : {}) }) };
    assert.deepEqual(f.writes().map(row => ({ name: row.name, body: row.args.body })),
      [{ name: action === 'fade_pause' ? 'setFadePause' : 'setSleep', body: expected }]);
    assert.equal(f.locks[0].length, 2); assert.equal(f.api.state.result.complete, true);
    assert.match(f.api.state.result.detail, /East@S1/); assert.match(f.api.state.result.detail, /East@S2/);
    assert.equal(await f.api.confirm(p), false); assert.equal(f.writes().length, 1);
    assert.equal(f.calls.filter(row => row.name === 'resolveRooms').length, 4);
    assert.equal(f.calls.filter(row => row.name === 'state').length, 4);
    assert.equal(f.calls.filter(row => row.name === 'setSleepRead').length, 4);
  });
}

for (const target of ['all', 'everywhere', 'all@S1', 'all@S2', 'here', 'Side@S1']) {
  test(`${target} uses actual resolver membership and accepts the single-group timer shape`, async () => {
    const f = fixture(), p = await f.review('sleep_set', target);
    assert.equal(await f.api.confirm(p), true, f.api.state.result?.detail);
    assert.equal(f.writes()[0].args.body.zone, target);
    assert.equal(f.api.state.result.targets.length, ['all', 'everywhere'].includes(target) ? 2 : 1);
  });
}

test('mixed timer states extend only eligible groups and preserve missing-timer failures', async () => {
  const f = fixture(); f.timers.delete('East@S2'); const p = await f.review('sleep_extend');
  assert.match(p.sources[1].label, /No timer to extend.*other groups continue/);
  assert.equal(await f.api.confirm(p), true); const result = f.api.state.result;
  assert.equal(result.partial, true); assert.equal(result.targets[0].status, 'confirmed');
  assert.equal(result.targets[1].status, 'failed'); assert.match(result.detail, /No active sleep timer/);
  assert.equal(f.writes().length, 1);
});

test('cancelling a room set includes groups with no timer as confirmed unchanged results', async () => {
  const f = fixture(); f.timers.clear(); const p = await f.review('sleep_cancel');
  assert.ok(p.sources.every(row => /No timer to cancel/.test(row.label)));
  assert.equal(await f.api.confirm(p), true); assert.equal(f.api.state.result.complete, true); assert.equal(f.api.state.result.changed, false);
});

test('fade review includes paused and stopped groups without pretending they need playback', async () => {
  for (const state of ['paused', 'stopped']) {
    const f = fixture(); f.rows.get('East@S2').transport_state = state; f.rows.get('East@S2').track = null;
    const p = await f.review('fade_pause'); assert.match(p.sources[1].label, /No current track/);
    assert.match(p.sources[1].label, /Paused or stopped groups stay unchanged/);
    assert.equal(await f.api.confirm(p), true); assert.match(f.api.state.result.detail, /Already paused/);
  }
});

test('ordinary timer countdown and source position advancement do not retire a reviewed bedtime action', async () => {
  for (const fades of [true, false]) {
    const f = fixture(); for (const timer of f.timers.values()) timer.fades = fades;
    const p = await f.review('sleep_extend');
    for (const timer of f.timers.values()) {
      timer.remaining_secs -= 10;
      if (!fades) timer.ends = new Date(Date.parse(timer.ends) + 1500).toISOString();
    }
    for (const row of f.rows.values()) row.track.position_secs += 10;
    assert.equal(await f.api.confirm(p), true, f.api.state.result?.detail);
  }
});

test('changed timers, fade phases, source occurrences, playback status and physical audience block dispatch', async () => {
  for (const edit of [f => { f.timers.get('East@S1').ends = '2030-01-01T02:00:00Z'; },
    f => { f.timers.get('East@S1').fading = true; }, f => f.timers.delete('East@S1'),
    f => { f.rows.get('East@S2').track.queue_position++; }, f => { f.rows.get('East@S2').track.uri = 'fixture:other'; },
    f => { f.rows.get('East@S1').transport_status = 'ERROR_OCCURRED'; },
    f => { f.physical = 'changed-address-or-bonding'; }, f => { f.aliases.bedtime = ['Side@S1']; }]) {
    const f = fixture(), p = await f.review(); edit(f);
    assert.equal(await f.api.confirm(p), false); assert.equal(f.writes().length, 0); assert.equal(f.api.state.result.status, 'not_sent');
  }
});

test('resolution is checked on both sides of the per-group reads', async () => {
  for (const confirming of [false, true]) {
    const f = fixture(); let p;
    if (confirming) p = await f.review(); else { f.api.open(); f.api.edit('input', 'bedtime'); f.api.edit('action', 'sleep_set'); }
    f.custom = name => { if (name === 'setSleepRead') f.physical = 'address-changed-during-read'; };
    assert.equal(await (confirming ? f.api.confirm(p) : f.api.review()), false);
    assert.equal(f.writes().length, 0);
  }
});

test('missing or malformed resolver revisions and timer observations cannot authorize a write', async () => {
  for (const revision of [undefined, 'old', 'A'.repeat(64), 'g'.repeat(64)]) {
    const f = fixture(); f.custom = (name, args) => name === 'resolveRooms' ? { ...f.resolve(args.query.target), revision } : undefined;
    f.api.open(); f.api.edit('input', 'all'); f.api.edit('action', 'sleep_set');
    assert.equal(await f.api.review(), false); assert.equal(f.writes().length, 0);
  }
  for (const list of [null, {}, [{ ...timer('wrong') }], [timer('East'), timer('East')], [timer('East', -1)]]) {
    const f = fixture(); f.custom = name => name === 'setSleepRead' ? list : undefined;
    f.api.open(); f.api.edit('input', 'all'); f.api.edit('action', 'sleep_cancel');
    assert.equal(await f.api.review(), false); assert.equal(f.writes().length, 0);
  }
});

test('timer and fade drafts are validated before discovery and cancel ignores an unused duration', async () => {
  for (const [action, field, value] of [['sleep_set', 'duration', '0'], ['sleep_extend', 'duration', '24h'],
    ['sleep_set', 'duration', '1.5m'], ['fade_pause', 'seconds', '0'], ['fade_pause', 'seconds', '61'], ['fade_pause', 'seconds', '2.5']]) {
    const f = fixture(); f.api.open(); f.api.edit('input', 'all'); f.api.edit('action', action); f.api.edit(field, value);
    assert.equal(await f.api.review(), false); assert.equal(f.calls.length, 0);
  }
  const f = fixture(); f.api.open(); f.api.edit('input', 'all'); f.api.edit('action', 'sleep_cancel'); f.api.edit('duration', 'bad');
  assert.equal(await f.api.review(), true);
});

test('bedtime observations bound both playback and timer reads to four in flight', async () => {
  const f = fixture(); f.zones = Array.from({ length: 9 }, (_, i) => ({ household: 'S1', coordinator_room: 'Room ' + i, members: ['Room ' + i] }));
  f.rows = new Map(f.zones.map(zone => [C.key(zone), { zone, transport_state: 'stopped', track: null }]));
  f.timers.clear(); let active = 0, maximum = 0;
  f.custom = (name, args) => {
    if (!['state', 'setSleepRead'].includes(name)) return undefined;
    return (async () => { active++; maximum = Math.max(maximum, active); await tick(); active--; return f.read(name, args); })();
  };
  await f.review('sleep_set', 'all'); assert.equal(maximum, 4); assert.equal(active, 0);
});

test('partial, all-failed and uncertain timer receipts retain every original group', async () => {
  for (const count of [1, 2]) for (const uncertain of [false, true]) {
    const f = fixture(), p = await f.review();
    f.custom = (name, args) => {
      if (name !== 'setSleep') return undefined;
      const result = f.result(name, args.body); result.failed = result.applied.splice(-count).map(row => ({ household: row.household,
        coordinator: row.coordinator, zone: row.timer.zone, error: error(uncertain ? 'OUTCOME_UNCERTAIN' : 'PLAYER_UNREACHABLE') }));
      result.complete = false; result.changed = result.applied.some(row => row.timer.changed); return result;
    };
    assert.equal(await f.api.confirm(p), true); assert.equal(f.api.state.result.targets.length, 2);
    assert.equal(f.api.state.result.partial, true); assert.equal(f.api.state.result.uncertain, uncertain);
    assert.match(f.api.state.result.detail, /East@S1/); assert.match(f.api.state.result.detail, /East@S2/);
    assert.equal(await f.api.confirm(p), false); assert.equal(f.writes().length, 1);
  }
});

test('multiline timer failure details survive without losing per-group uncertainty', async () => {
  const f = fixture(), p = await f.review();
  f.custom = (name, args) => {
    if (name !== 'setSleep') return undefined;
    const result = f.result(name, args.body), row = result.applied.pop();
    result.failed = [{ household: row.household, coordinator: row.coordinator, zone: row.timer.zone,
      error: { ...error('OUTCOME_UNCERTAIN'), detail: 'The speaker accepted the request.\nThe acknowledgement was lost.\tRead its timer.' } }];
    result.complete = false; return result;
  };
  assert.equal(await f.api.confirm(p), true); assert.equal(f.api.state.result.uncertain, true);
  assert.match(f.api.state.result.detail, /accepted the request\.\nThe acknowledgement was lost\.\tRead its timer/);
  assert.equal(f.api.state.result.targets[0].status, 'confirmed'); assert.equal(f.writes().length, 1);
});

test('missing, duplicated, alien and contradictory timer receipts stay uncertain without replay', async () => {
  const edits = [v => { v.applied.pop(); }, v => { v.applied[1] = copy(v.applied[0]); },
    v => { v.applied[0].coordinator = 'unreviewed'; }, v => { v.applied[0].timer.zone = 'unreviewed'; },
    v => { v.complete = false; }, v => { v.changed = false; }, v => { v.applied[0].timer.remaining_secs++; },
    v => { v.applied[0].timer.fading = true; }, v => { v.applied[0].timer.changed = false; }];
  for (const edit of edits) {
    const f = fixture(), p = await f.review(); f.custom = (name, args) => {
      if (name !== 'setSleep') return undefined; const result = f.result(name, args.body); edit(result); return result;
    };
    assert.equal(await f.api.confirm(p), false); assert.equal(f.api.state.result.status, 'uncertain');
    assert.equal(await f.api.confirm(p), false); assert.equal(f.writes().length, 1);
  }
});

test('fade failures retain structured per-group evidence from both successful and failed HTTP responses', async () => {
  for (const failedHTTP of [false, true]) {
    const f = fixture(), p = await f.review('fade_pause');
    f.custom = (name, args) => {
      if (name !== 'setFadePause') return undefined;
      const result = f.result(name, args.body); result.batch.complete = false;
      result.batch.results[0].result = { status: 'failed', error: error('OUTCOME_UNCERTAIN') };
      if (failedHTTP) { result.batch.results[1].result = { status: 'not_attempted' }; throw Object.assign(new Error('Fade failed'), { batch: result.batch }); }
      return result;
    };
    assert.equal(await f.api.confirm(p), !failedHTTP); assert.equal(f.api.state.result.partial, true);
    assert.equal(f.api.state.result.uncertain, true); assert.equal(f.api.state.result.targets.length, 2);
    assert.equal(f.writes().length, 1); assert.match(f.api.state.result.detail, /not repeat|not repeated/);
  }
});

test('editing or closing during a delayed review cancels it, and a double confirmation dispatches once', async () => {
  for (const retire of [api => api.edit('duration', '20m'), api => api.edit('seconds', '20'), api => api.close(), api => api.dispose()]) {
    const f = fixture(), gate = deferred();
    f.api.open(); f.api.edit('input', 'bedtime'); f.api.edit('action', 'sleep_set');
    f.custom = name => name === 'setSleepRead' ? gate.promise : undefined;
    const reviewing = f.api.review(); await until(() => f.calls.some(row => row.name === 'setSleepRead'));
    retire(f.api); assert.equal(await f.api.review(), false);
    gate.resolve([timer('East')]); assert.equal(await reviewing, false); assert.equal(f.api.state.preview, null); assert.equal(f.writes().length, 0);
  }
  const f = fixture({ execute: async (_, work) => Promise.all([work(), work()]) }), p = await f.review();
  await Promise.all([f.api.confirm(p), f.api.confirm(p)]); assert.equal(f.writes().length, 1);
});

test('a sent request keeps its receipt after the panel closes and never permits an automatic replay', async () => {
  const f = fixture(), p = await f.review(), gate = deferred();
  f.custom = name => name === 'setSleep' ? gate.promise : undefined;
  const sending = f.api.confirm(p); await until(() => f.writes().length === 1);
  f.api.close(); f.api.open(); assert.equal(f.api.edit('input', 'all'), false); assert.equal(await f.api.confirm(p), false);
  gate.resolve(f.result('setSleep', p.request.body)); assert.equal(await sending, true);
  assert.equal(f.api.state.result.complete, true); assert.equal(f.writes().length, 1);
});

function documentFixture() {
  const roots = new Map(), events = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { hidden: false, activeElement: null, createTextNode: text => String(text),
    addEventListener(name, fn) { events.set(name, fn); }, removeEventListener(name) { events.delete(name); },
    fire(name) { events.get(name)?.(); }, getElementById: id => roots.get(id) || null,
    createElement(tag) {
      const listeners = new Map(), node = { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', disabled: false, hidden: false, open: false, parent: null, _text: '',
        get firstChild() { return this.children[0]; },
        get textContent() { return this._text + this.children.map(child => typeof child === 'object' ? child.textContent : child).join(' '); },
        set textContent(value) { this._text = String(value); this.replaceChildren(); },
        append(...children) { for (const child of children.flat()) if (child != null) { if (typeof child === 'object') child.parent = this; this.children.push(child); } },
        replaceChildren(...children) { this.children = []; this.append(...children); },
        setAttribute(name, value) { this.attributes[name] = String(value); if (['id', 'type', 'value'].includes(name)) this[name] = String(value); if (name === 'id') roots.set(String(value), this); },
        getAttribute(name) { return this.attributes[name] ?? null; },
        addEventListener(name, fn) { const list = listeners.get(name) || []; list.push(fn); listeners.set(name, list); },
        removeEventListener(name, fn) { listeners.set(name, (listeners.get(name) || []).filter(value => value !== fn)); },
        fire(name) { if (!this.disabled) for (const fn of listeners.get(name) || []) fn({ target: this, preventDefault() {} }); },
        querySelectorAll(selector) { return descendants(this).filter(child => selector.startsWith('[')
          ? child.getAttribute(selector.slice(1, -1)) !== null : child.tagName === selector.toUpperCase()); },
        querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; },
      }; return node;
    } };
  return doc;
}
const button = (node, text) => node.querySelectorAll('button').find(node => node.textContent === text);
const field = (node, label) => node.querySelectorAll('[aria-label]').find(node => node.getAttribute('aria-label') === label);
function phonePanel(f, doc) {
  const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const el = vm.runInNewContext(source.slice(source.indexOf('function el('), source.indexOf('\nlet toastTimer')) + '\nel;', { document: doc });
  return Panel.mount({ api: Sets, call: f.call, el, document: doc, execute: async (groups, work) => { f.locks.push(groups); return work(); } });
}
async function choose(panel, action) {
  const details = panel.node.querySelector('details'); details.open = true; details.fire('toggle');
  await until(() => !panel.state.reading);
  const target = field(panel.node, 'Room or alias to control'); target.value = 'bedtime'; target.fire('input');
  const select = field(panel.node, 'Collective room action'); select.value = action; select.fire('change');
  const duration = field(panel.node, 'Room-set sleep duration'); duration.value = '15m'; duration.fire('input');
  const seconds = field(panel.node, 'Room-set fade seconds'); seconds.value = '10'; seconds.fire('input');
  button(panel.node, 'Resolve and review').fire('click'); await until(() => !!panel.state.preview);
}

for (const action of ['sleep_set', 'sleep_extend', 'sleep_cancel', 'fade_pause']) {
  test(`phone actual room-set controls review and submit ${action}`, async t => {
    const f = fixture(), doc = documentFixture(), panel = phonePanel(f, doc); t.after(() => panel.dispose());
    await choose(panel, action); assert.match(panel.node.textContent, /East@S1/); assert.match(panel.node.textContent, /East@S2/);
    assert.equal(field(panel.node, 'Room-set sleep duration').parent.hidden, !['sleep_set', 'sleep_extend'].includes(action));
    assert.equal(field(panel.node, 'Room-set fade seconds').parent.hidden, action !== 'fade_pause');
    assert.equal(f.writes().length, 0); const confirm = button(panel.node, 'Confirm collective action'); confirm.fire('click'); confirm.fire('click');
    await until(() => !panel.state.writing && f.writes().length === 1);
    assert.equal(f.writes()[0].args.body.expected_revision.length, 64); assert.equal(panel.state.result.complete, true);
    assert.equal(f.locks[0].length, 2);
  });
}

test('phone destination/duration edits and hiding retire reviews while retaining the original receipt', async t => {
  const f = fixture(), doc = documentFixture(), panel = phonePanel(f, doc); t.after(() => panel.dispose());
  await choose(panel, 'sleep_set'); const prior = panel.state.preview;
  const duration = field(panel.node, 'Room-set sleep duration'); duration.value = '20m'; duration.fire('input');
  assert.equal(panel.state.preview, null); assert.equal(await panel.confirm(prior), false);
  await choose(panel, 'sleep_set'); doc.hidden = true; doc.fire('visibilitychange');
  assert.equal(panel.state.open, false); assert.equal(panel.state.preview, null); assert.equal(f.writes().length, 0);
});

async function dashboardFixture() {
  const f = fixture(), doc = documentFixture();
  const html = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/dashboard.html'), 'utf8');
  for (const [, id] of html.matchAll(/\bid="([^"]+)"/g)) { const node = doc.createElement(id === 'sleep-dialog' ? 'dialog' : 'div'); node.setAttribute('id', id); }
  let sleep;
  const controller = C.model({ call: f.urlCall, changed: () => sleep?.observe() });
  sleep = Sleep.panel({ call: f.urlCall, controller, document: doc });
  assert.equal(await controller.refresh(), true); controller.connection(true);
  doc.getElementById('open-sleep').fire('click'); await until(() => sleep.state.loaded);
  return Object.assign(f, { doc, controller, sleep, panel: sleep.roomSets });
}
for (const action of ['sleep_set', 'sleep_extend', 'sleep_cancel', 'fade_pause']) {
  test(`dashboard sleep dialog mounts shared ${action} controls and uses the existing whole-house lane`, async t => {
    const f = await dashboardFixture(); t.after(() => f.sleep.dispose());
    assert.ok(f.panel); assert.equal(f.doc.getElementById('sleep-dialog').open, true);
    assert.deepEqual(field(f.panel.node, 'Collective room action').children.map(node => node.value), ['sleep_set', 'sleep_extend', 'sleep_cancel', 'fade_pause']);
    await choose(f.panel, action); const gate = deferred(), p = f.panel.state.preview;
    f.custom = name => ['setSleep', 'setFadePause'].includes(name) ? gate.promise : undefined;
    button(f.panel.node, 'Confirm collective action').fire('click'); await until(() => f.writes().length === 1);
    assert.equal(f.controller.state.busy, true); assert.equal(await f.controller.command('next'), false);
    assert.equal(f.sleep.review('set'), null);
    gate.resolve(f.result(action === 'fade_pause' ? 'setFadePause' : 'setSleep', p.request.body));
    await until(() => !f.panel.state.writing && !f.controller.state.busy);
    assert.equal(f.panel.state.result.complete, true); assert.equal(f.writes().length, 1);
    assert.match(f.panel.node.textContent, /East@S1/); assert.match(f.panel.node.textContent, /East@S2/);
  });
}

test('dashboard closing a delayed bedtime confirmation sends nothing; a sent receipt survives closing', async t => {
  for (const afterSend of [false, true]) {
    const f = await dashboardFixture(); t.after(() => f.sleep.dispose()); await choose(f.panel, 'sleep_set');
    const gate = deferred(), p = f.panel.state.preview, initialReads = f.calls.filter(row => row.name === 'setSleepRead').length;
    f.custom = name => name === (afterSend ? 'setSleep' : 'setSleepRead') ? gate.promise : undefined;
    const sending = f.panel.confirm(p);
    await until(() => afterSend ? f.writes().length === 1 : f.calls.filter(row => row.name === 'setSleepRead').length > initialReads);
    f.doc.getElementById('close-sleep').fire('click');
    gate.resolve(afterSend ? f.result('setSleep', p.request.body) : []); await sending;
    assert.equal(f.writes().length, afterSend ? 1 : 0);
    if (afterSend) assert.equal(f.panel.state.result.complete, true);
    assert.equal(f.doc.getElementById('sleep-dialog').open, false);
  }
});

test('both shipped clients use the long bedtime write deadline without extending GET or unrelated writes', async () => {
  const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const request = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\nRequestRemote;', { AbortController, URLSearchParams, setTimeout, clearTimeout });
  for (const route of ['/sleep', '/pause/fade']) {
    assert.equal(request.timeoutFor('POST', route), 600000); assert.equal(request.timeoutFor('POST', route + '?trace=1'), 600000);
    assert.equal(request.timeoutFor('GET', route), 15000); assert.equal(request.timeoutFor('POST', route + '/extra'), 120000);
  }
  const budgets = [], call = C.request({ fetcher: async () => new Response('{}'), schedule(fn, ms) { budgets.push(ms); return 1; }, cancel() {} });
  for (const route of ['/sleep', '/pause/fade']) { await call(route, { zone: 'all' }); await call(route); await call(route + '/extra', {}); }
  assert.deepEqual(budgets, [600000, 10000, 120000, 600000, 10000, 120000]);
  const bundle = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote/dashboard.rs'), 'utf8');
  assert.ok(bundle.includes('{ROOM_SETS_PANEL}')); assert.ok(bundle.includes('{ROOM_SETS_STYLE}'));
  assert.ok(bundle.indexOf('{ROOM_SETS}') < bundle.indexOf('{ROOM_SETS_PANEL}'));
});

test('real HTTP bedtime writes preserve mixed receipts and lost acknowledgements are never repeated', { timeout: 10000 }, async t => {
  const f = fixture(); let posts = 0, lose = false;
  const server = http.createServer((req, res) => {
    let data = ''; req.on('data', chunk => { data += chunk; }); req.on('end', async () => {
      if (req.method === 'POST') posts++;
      if (req.method === 'POST' && lose) { req.socket.destroy(); return; }
      try {
        const value = await f.urlCall(req.url, req.method === 'POST' ? JSON.parse(data) : undefined);
        res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(value));
      } catch (e) { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify(error())); }
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));
  const base = 'http://127.0.0.1:' + server.address().port;
  const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const call = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '') + '\nObject.assign(ROUTES, RoomSetsRemote.routes); call;', {
    AbortController, URLSearchParams, setTimeout, clearTimeout, RoomSetsRemote: Sets, fetch: (url, init) => fetch(base + url, init) });
  const api = Sets.model({ call }); api.open(); api.edit('input', 'bedtime'); api.edit('action', 'sleep_extend'); api.edit('duration', '15m');
  f.timers.delete('East@S2'); assert.equal(await api.review(), true); assert.equal(await api.confirm(api.state.preview), true);
  assert.equal(api.state.result.partial, true); assert.equal(api.state.result.targets[1].status, 'failed'); assert.equal(posts, 1);
  api.edit('action', 'sleep_set'); assert.equal(await api.review(), true); lose = true;
  const p = api.state.preview; assert.equal(await api.confirm(p), false); assert.equal(api.state.result.status, 'uncertain');
  assert.equal(await api.confirm(p), false); assert.equal(posts, 2);
});
