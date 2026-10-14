// No packages needed: node --test scripts/test-remote-play-mode.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const Mode = require('../crates/fsonos-api/src/remote/play-mode.js');
const group = (household = 'S2', room = 'Room', members = [room]) => ({
  household, coordinator_room: room, members, transport_state: 'paused',
});
const view = (mode = 'NORMAL', extra = {}) => ({ zone: 'Room', mode, queue_active: true, dj_active: false, ...extra });
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture(options = {}) {
  let houses = options.groups || [group()];
  let setting = options.setting || view();
  const calls = [];
  const model = Mode.model({ call: async (name, args) => {
    calls.push([name, args]);
    if (name === 'zones') return options.zonesRead ? options.zonesRead() : houses;
    if (name === 'modeRead') return options.modeRead ? options.modeRead() : setting;
    assert.equal(name, 'modeWrite', 'no playback, queue or other setting mutation');
    if (options.fail) throw options.fail;
    if (Object.hasOwn(options, 'answer')) return typeof options.answer === 'function' ? options.answer(args.body) : options.answer;
    const old = setting.mode;
    setting = { ...setting, mode: args.body.mode };
    return { done: 'Playback order changed', state: setting, changed: old !== setting.mode, recovered: false };
  } });
  return { model, calls, groups(value) { houses = value; }, mode(value) { setting = value; } };
}
async function ready(f) { assert.equal(await f.model.refresh(), true); assert.equal(await f.model.read('Room@S2'), true); }

test('six complete wire modes and route registration agree with the embedded assets', () => {
  assert.deepEqual(Object.keys(Mode.modes).sort(), ['NORMAL', 'REPEAT_ALL', 'REPEAT_ONE', 'SHUFFLE', 'SHUFFLE_NOREPEAT', 'SHUFFLE_REPEAT_ONE']);
  assert.deepEqual(Mode.routes, { modeRead: ['GET', '/zones/{room}/play-mode'], modeWrite: ['POST', '/play-mode'] });
  const rust = readFileSync(join(__dirname, '../crates/fsonos-api/src/http/remote.rs'), 'utf8');
  assert.ok(rust.includes('include_str!("../remote/play-mode.js")'));
  assert.ok(rust.includes('{TIMERS_SCRIPT}\\n{MODE_SCRIPT}'));
  assert.ok(rust.includes('{TIMERS_STYLE}\\n{MODE_STYLE}'));
  assert.ok(rust.includes('("play-mode.js", MODE_SCRIPT)'));
});

test('lazy reads qualify otherwise identical household room names', async () => {
  const f = fixture({ groups: [group('S1'), group('S2')] });
  assert.equal(f.calls.length, 0); await ready(f);
  assert.deepEqual(f.model.state.groups.map((entry) => entry.target), ['Room@S1', 'Room@S2']);
  assert.deepEqual(f.calls[1], ['modeRead', { params: { room: 'Room@S2' } }]);
});

test('every distinct pair of modes sends one complete preconditioned change', async () => {
  for (const before of Object.keys(Mode.modes)) for (const wanted of Object.keys(Mode.modes)) {
    const f = fixture({ setting: view(before) }); await ready(f);
    assert.equal(f.model.choose(wanted), true);
    assert.equal(f.model.canApply(), before !== wanted);
    assert.equal(await f.model.apply(), before !== wanted);
    const writes = f.calls.filter(([name]) => name === 'modeWrite');
    assert.equal(writes.length, before === wanted ? 0 : 1);
    if (before !== wanted) {
      assert.deepEqual(writes[0][1], { body: { zone: 'Room@S2', mode: wanted, expected_mode: before } });
      assert.equal(f.model.state.current.mode, wanted);
      assert.equal(f.model.canApply(), false);
    }
  }
});

test('a paused but selected queue is writable without resume or source switching', async () => {
  const f = fixture(); await ready(f); f.model.choose('SHUFFLE_NOREPEAT');
  assert.equal(await f.model.apply(), true);
  assert.deepEqual(f.calls.map(([name]) => name), ['zones', 'modeRead', 'zones', 'modeRead', 'modeWrite']);
});

test('another source remains read-only and is never activated by a mode change', async () => {
  const f = fixture({ setting: view('NORMAL', { queue_active: false }) }); await ready(f);
  assert.equal(f.model.choose('SHUFFLE'), false); assert.equal(await f.model.apply(), false);
  assert.equal(f.calls.length, 2);
});

test('an active DJ permits only normalization, never shuffle or repeat', async () => {
  const f = fixture({ setting: view('SHUFFLE', { dj_active: true }) }); await ready(f);
  for (const mode of Object.keys(Mode.modes).filter((mode) => mode !== 'NORMAL')) assert.equal(f.model.choose(mode), false);
  assert.equal(f.model.choose('NORMAL'), true); assert.equal(await f.model.apply(), true);
  assert.equal(f.model.state.current.mode, 'NORMAL');
});

test('unknown modes or incomplete reads never become an assumed normal setting', async () => {
  for (const setting of [null, {}, view('FUTURE_MODE'), view('NORMAL', { queue_active: undefined }),
    view('NORMAL', { dj_active: undefined }), view('NORMAL', { zone: 'Other Room' })]) {
    const f = fixture({ modeRead: async () => setting });
    await f.model.refresh(); assert.equal(await f.model.read('Room@S2'), false);
    assert.equal(f.model.state.current, null); assert.equal(f.model.choose('NORMAL'), false);
    assert.equal(await f.model.apply(), false);
  }
});

test('invalid, prototype and lowercase desired modes are not sent', async () => {
  const f = fixture(); await ready(f);
  for (const mode of ['shuffle', '', null, 'toString', '__proto__', 'FUTURE_MODE']) assert.equal(f.model.choose(mode), false);
  assert.equal(await f.model.apply(), false); assert.equal(f.calls.length, 2);
});

test('membership changes prevent dispatch even with unchanged mode', async () => {
  const f = fixture(); await ready(f); f.model.choose('REPEAT_ONE');
  f.groups([group('S2', 'Room', ['Room', 'New Room'])]);
  assert.equal(await f.model.apply(), false); assert.equal(f.model.state.current, null);
  assert.match(f.model.state.message, /No mode change was sent/);
  assert.equal(f.calls.filter(([name]) => name === 'modeWrite').length, 0);
});

test('mode, source and DJ changes in preflight invalidate the observed setting', async () => {
  for (const changed of [view('REPEAT_ALL'), view('NORMAL', { queue_active: false }), view('NORMAL', { dj_active: true })]) {
    const f = fixture(); await ready(f); f.model.choose('SHUFFLE'); f.mode(changed);
    assert.equal(await f.model.apply(), false); assert.equal(await f.model.apply(), false);
    assert.equal(f.calls.filter(([name]) => name === 'modeWrite').length, 0);
    assert.equal(f.model.state.current, null);
  }
});

test('normal membership ordering changes do not reject the same group', async () => {
  const f = fixture({ groups: [group('S2', 'Room', ['Room', 'A', 'B'])] }); await ready(f);
  f.groups([group('S2', 'Room', ['B', 'Room', 'A'])]); f.model.choose('REPEAT_ALL');
  assert.equal(await f.model.apply(), true);
});

test('closed preflights send nothing and ignore late reads', async () => {
  const f = fixture(); await ready(f); f.model.choose('SHUFFLE');
  const wait = deferred(); const original = f.model;
  // A separate controller lets the preflight group read remain unresolved.
  let reads = 0; const calls = [];
  const c = Mode.model({ call: async (name) => {
    calls.push(name);
    if (name === 'zones') return ++reads === 1 ? [group()] : wait.promise;
    if (name === 'modeRead') return view();
    throw new Error('a closed preflight must not dispatch');
  } });
  await c.refresh(); await c.read('Room@S2'); c.choose('SHUFFLE');
  const changing = c.apply(); c.close(); wait.resolve([group()]);
  assert.equal(await changing, false); assert.equal(c.state.current, null);
  assert.deepEqual(calls, ['zones', 'modeRead', 'zones']);
  original.dispose();
});

test('duplicate apply and other requests cannot overlap a pending write', async () => {
  const wait = deferred(); const f = fixture({ answer: () => wait.promise }); await ready(f);
  f.model.choose('REPEAT_ALL'); const first = f.model.apply();
  await new Promise((done) => setImmediate(done));
  assert.equal(await f.model.apply(), false); assert.equal(await f.model.refresh(), false);
  assert.equal(await f.model.read(), false); assert.equal(f.model.choose('NORMAL'), false);
  wait.resolve({ done: 'Repeat enabled', state: view('REPEAT_ALL'), changed: true, recovered: false });
  assert.equal(await first, true); assert.equal(f.calls.filter(([name]) => name === 'modeWrite').length, 1);
});

test('lost and rejected responses are terminal until an explicit new read', async () => {
  for (const fail of [new Error('response lost'), Object.assign(new Error('denied'), { hint: 'Policy denied this action.' })]) {
    const f = fixture({ fail }); await ready(f); f.model.choose('SHUFFLE');
    assert.equal(await f.model.apply(), false); assert.equal(await f.model.apply(), false);
    assert.equal(f.model.state.current, null); assert.match(f.model.state.message, /not repeated/);
    assert.equal(f.calls.filter(([name]) => name === 'modeWrite').length, 1);
    assert.equal(await f.model.read(), true);
  }
});

test('a conflicting or malformed success does not enable a second write', async () => {
  const good = { done: 'Changed', state: view('SHUFFLE'), changed: true, recovered: false };
  for (const answer of [null, {}, { ...good, done: '' }, { ...good, state: view('NORMAL') },
    { ...good, state: view('SHUFFLE', { queue_active: false }) }, { ...good, state: view('SHUFFLE', { dj_active: true }) },
    { ...good, recovered: undefined }, { ...good, changed: 'yes' }]) {
    const f = fixture({ answer }); await ready(f); f.model.choose('SHUFFLE');
    assert.equal(await f.model.apply(), false); assert.equal(f.model.state.current, null);
    assert.equal(await f.model.apply(), false); assert.equal(f.calls.filter(([name]) => name === 'modeWrite').length, 1);
  }
});

test('read-back recovery is accepted without resending the mode', async () => {
  const f = fixture({ answer: { done: 'Confirmed by read-back', state: view('SHUFFLE'), changed: true, recovered: true } });
  await ready(f); f.model.choose('SHUFFLE'); assert.equal(await f.model.apply(), true);
  assert.match(f.model.state.message, /read-back/); assert.equal(f.model.canApply(), false);
});

test('slow old mode reads cannot replace a newer destination', async () => {
  const first = deferred(), second = deferred(); let n = 0;
  const f = fixture({ groups: [group('S1'), group('S2')], modeRead: () => (++n === 1 ? first : second).promise });
  await f.model.refresh(); const a = f.model.read('Room@S1'), b = f.model.read('Room@S2');
  second.resolve(view('REPEAT_ONE')); await b;
  first.resolve(view('NORMAL')); await a;
  assert.equal(f.model.state.target, 'Room@S2'); assert.equal(f.model.state.current.mode, 'REPEAT_ONE');
});

test('malformed or overlapping groups are never offered as destinations', async () => {
  for (const value of [null, {}, [group('S2', 'Room', [])], [group(), group()],
    [group('S2', 'Room', ['Room', 'Other']), group('S2', 'Other')]]) {
    const f = fixture({ zonesRead: async () => value });
    assert.equal(await f.model.refresh(), false); assert.deepEqual(f.model.state.groups, []);
    assert.equal(await f.model.read('Room@S2'), false);
  }
});

test('refresh requires a new mode read and drops a regrouped destination', async () => {
  const f = fixture(); await ready(f); f.model.choose('SHUFFLE');
  await f.model.refresh(); assert.equal(f.model.state.current, null); assert.equal(await f.model.apply(), false);
  assert.equal(f.model.state.target, 'Room@S2');
  f.groups([group('S2', 'Room', ['Room', 'Other'])]); await f.model.refresh();
  assert.equal(f.model.state.target, '');
});

test('closed or disposed controllers cannot send changes or resurrect late reads', async () => {
  for (const end of ['close', 'dispose']) {
    const wait = deferred(); const f = fixture({ modeRead: () => wait.promise });
    await f.model.refresh(); const reading = f.model.read('Room@S2'); f.model[end]();
    wait.resolve(view()); assert.equal(await reading, false); assert.equal(f.model.state.current, null);
    assert.equal(f.model.choose('SHUFFLE'), false); assert.equal(await f.model.apply(), false);
  }
});
