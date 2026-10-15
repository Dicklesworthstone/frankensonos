'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const Equalizer = require('../crates/fsonos-api/src/remote/equalizer.js');

const zones = () => [
  { household: 'home', coordinator_room: 'Kitchen', members: ['Kitchen', 'Study'] },
  { household: 'other', coordinator_room: 'Kitchen', members: ['Kitchen'] },
];
const eq = (room = 'Study', changes = {}) => ({ room, bass: 2, treble: -1, loudness: true, ...changes });
const clone = (value) => structuredClone(value);
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const tick = () => new Promise((resolve) => setImmediate(resolve));

function fixture() {
  const calls = [];
  let inventory = zones(), sound = eq(), handler = null, publications = 0;
  const model = Equalizer.model({
    changed: () => { publications++; },
    call: async (route, args) => {
      calls.push({ route, args: clone(args) });
      if (handler) return handler(route, args);
      if (route === 'zones') return clone(inventory);
      if (route === 'eqRead') return clone(sound);
      if (route === 'eqWrite') {
        const { zone, ...patch } = args.body;
        sound = { ...sound, ...patch };
        return { done: 'Sound changed.', eq: clone(sound) };
      }
      throw new Error(`Unexpected route: ${route}`);
    },
  });
  return { model, calls,
    inventory(value) { inventory = value; }, sound(value) { sound = value; },
    handle(value) { handler = value; }, publications: () => publications,
    writes: () => calls.filter((call) => call.route === 'eqWrite'),
    async ready(target = 'Study@home') { await model.refresh(); await model.read(target); return model; },
  };
}

test('opening is lazy, lists every speaker and disambiguates households without autoselection', async () => {
  const f = fixture();
  assert.deepEqual(f.calls, []);
  assert.equal(f.model.state.open, false);
  assert.equal(await f.model.apply(), false);
  assert.equal(await f.model.refresh(), true);
  assert.deepEqual(f.model.state.rooms.map((room) => room.target), ['Kitchen@home', 'Study@home', 'Kitchen@other']);
  assert.equal(f.model.state.target, '');
  assert.equal(f.model.state.current, null);
  assert.equal(f.calls.length, 1);
});

test('a noncoordinator is read and changed by its qualified room, not the group coordinator', async () => {
  const f = fixture(), m = await f.ready();
  assert.deepEqual(f.calls[1], { route: 'eqRead', args: { params: { room: 'Study@home' } } });
  assert.equal(m.canApply(), false);
  assert.equal(await m.apply(), false);
  assert.equal(m.choose('bass', -10), true);
  assert.equal(f.calls.length, 2, 'editing cannot send commands');
  assert.equal(m.canApply(), true);
  assert.equal(await m.apply(), true);
  assert.deepEqual(f.calls.slice(2).map((call) => call.route), ['zones', 'eqRead', 'eqWrite']);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Study@home', bass: -10 });
  assert.equal(m.state.current.bass, -10);
  assert.equal(m.state.current.treble, -1);
  assert.equal(m.state.current.loudness, true);
  assert.equal(m.canApply(), false);
  assert.equal(await m.apply(), false);
  assert.equal(f.writes().length, 1, 'an acknowledged setting is not resent');
});

test('zero levels and false loudness survive the combined patch; flat and discard stay local', async () => {
  const f = fixture(), m = await f.ready();
  assert.equal(m.flat(), true);
  assert.deepEqual(m.state.draft, { bass: 0, treble: 0, loudness: false });
  assert.equal(m.canApply(), true);
  assert.equal(m.discard(), true);
  assert.deepEqual(m.state.draft, { bass: 2, treble: -1, loudness: true });
  assert.equal(f.calls.length, 2);
  assert.equal(m.canApply(), false);
  m.flat();
  assert.equal(await m.apply(), true);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Study@home', bass: 0, treble: 0, loudness: false });
});

test('loudness alone never writes cached bass and treble', async () => {
  const f = fixture(), m = await f.ready();
  m.choose('loudness', false);
  assert.equal(await m.apply(), true);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Study@home', loudness: false });
});

test('a different household with the same room name remains a distinct destination', async () => {
  const f = fixture(); f.sound(eq('Kitchen'));
  const m = await f.ready('Kitchen@other');
  m.choose('treble', 10);
  assert.equal(await m.apply(), true);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Kitchen@other', treble: 10 });
});

test('only integer tone bounds and boolean loudness are accepted; no extra keys enter the body', async () => {
  const f = fixture(), m = await f.ready();
  for (const value of [-11, 11, 1.5, NaN, Infinity, -Infinity, '1', '', true, null, undefined]) {
    assert.equal(m.choose('bass', value), false, String(value));
    assert.equal(m.choose('treble', value), false, String(value));
  }
  for (const value of [0, 1, 'false', null, undefined]) assert.equal(m.choose('loudness', value), false);
  for (const key of ['room', 'zone', '__proto__', 'constructor', 'unexpected']) assert.equal(m.choose(key, 0), false);
  assert.equal(m.choose('bass', -10), true);
  assert.equal(m.choose('treble', 10), true);
  m.state.draft.bass = 11;
  assert.equal(m.canApply(), false);
  assert.equal(await m.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('each stale sound component blocks the entire write, including an unedited component', async () => {
  for (const change of [{ bass: 3 }, { treble: 0 }, { loudness: false }]) {
    const f = fixture(), m = await f.ready();
    m.choose('bass', 5); f.sound(eq('Study', change));
    assert.equal(await m.apply(), false);
    assert.equal(f.writes().length, 0);
    assert.equal(m.state.current, null);
    assert.equal(m.state.draft, null);
    assert.match(m.state.message, /sound changed/);
    assert.match(m.state.message, /No sound change was sent/);
    assert.equal(await m.apply(), false);
  }
});

test('room disappearance is checked before tone and before any write', async () => {
  const f = fixture(), m = await f.ready();
  m.choose('bass', 4); f.inventory([zones()[1]]);
  assert.equal(await m.apply(), false);
  assert.equal(f.writes().length, 0);
  assert.deepEqual(f.calls.slice(2).map((call) => call.route), ['zones']);
  assert.match(m.state.message, /no longer available/);
});

test('a regrouped speaker is still that speaker: tone does not demand the old coordinator', async () => {
  const f = fixture(), m = await f.ready();
  m.choose('bass', 4);
  f.inventory([{ household: 'home', coordinator_room: 'Study', members: ['Study', 'Kitchen'] }]);
  assert.equal(await m.apply(), true);
  assert.equal(f.writes()[0].args.body.zone, 'Study@home');
});

test('malformed, ambiguous and duplicate inventories fail closed', async () => {
  const invalid = [null, {}, [null], [{ ...zones()[0], household: '' }],
    [{ ...zones()[0], coordinator_room: 'Missing' }], [{ ...zones()[0], members: [] }],
    [{ ...zones()[0], members: ['Kitchen', 1] }], [{ ...zones()[0], members: ['Kitchen', 'Kitchen'] }],
    [zones()[0], zones()[0]],
    [{ household: 'b@c', coordinator_room: 'a', members: ['a'] },
      { household: 'c', coordinator_room: 'a@b', members: ['a@b'] }]];
  for (const value of invalid) {
    const f = fixture(); f.inventory(value);
    assert.equal(await f.model.refresh(), false, JSON.stringify(value));
    assert.deepEqual(f.model.state.rooms, []);
    assert.equal(f.model.state.target, '');
    assert.equal(f.model.state.loading, false);
    assert.equal(await f.model.read('Study@home'), false);
    assert.equal(f.writes().length, 0);
  }
});

test('an empty inventory is a valid explicit discovery state, not a fabricated default speaker', async () => {
  const f = fixture(); f.inventory([]);
  assert.equal(await f.model.refresh(), true);
  assert.deepEqual(f.model.state.rooms, []);
  assert.match(f.model.state.message, /No rooms found/);
  assert.equal(await f.model.read('Study@home'), false);
});

test('missing, wrong-room, out-of-range and ill-typed reads cannot enable controls', async () => {
  const invalid = [null, {}, eq('Kitchen'), eq('Study', { bass: -11 }), eq('Study', { treble: 11 }),
    eq('Study', { bass: 0.5 }), eq('Study', { treble: '1' }), eq('Study', { loudness: 1 }),
    eq('Study', { bass: undefined }), eq('Study', { loudness: undefined })];
  for (const value of invalid) {
    const f = fixture(); f.sound(value); await f.model.refresh();
    assert.equal(await f.model.read('Study@home'), false, JSON.stringify(value));
    assert.equal(f.model.state.current, null);
    assert.equal(f.model.canApply(), false);
    assert.equal(f.model.choose('bass', 2), false);
    assert.equal(f.model.flat(), false);
    assert.equal(f.model.discard(), false);
    assert.equal(f.model.state.loading, false);
  }
});

test('a newer room read wins over a late response for the previous room', async () => {
  const f = fixture(); await f.model.refresh();
  const old = deferred();
  f.handle((route, args) => args.params.room === 'Study@home' ? old.promise : eq('Kitchen', { bass: 7 }));
  const first = f.model.read('Study@home');
  assert.equal(await f.model.read('Kitchen@other'), true);
  old.resolve(eq());
  assert.equal(await first, false);
  assert.equal(f.model.state.target, 'Kitchen@other');
  assert.equal(f.model.state.current.bass, 7);
});

test('a refresh supersedes an in-flight read and discards all drafts even when the room remains', async () => {
  const f = fixture(), m = await f.ready();
  m.choose('bass', 8);
  const old = deferred();
  f.handle((route) => route === 'zones' ? zones() : old.promise);
  const reading = m.read();
  assert.equal(await m.refresh(), true);
  old.resolve(eq('Study', { bass: 8 }));
  assert.equal(await reading, false);
  assert.equal(m.state.target, 'Study@home');
  assert.equal(m.state.current, null);
  assert.equal(m.state.draft, null);
  assert.equal(m.canApply(), false);
});

test('out-of-order inventories cannot resurrect an old target or room list', async () => {
  const f = fixture(), old = deferred();
  f.handle(() => old.promise);
  const first = f.model.refresh();
  f.handle(() => [zones()[1]]);
  assert.equal(await f.model.refresh(), true);
  old.resolve(zones());
  assert.equal(await first, false);
  assert.deepEqual(f.model.state.rooms.map((room) => room.target), ['Kitchen@other']);
});

test('closing during read ignores its later response and does not begin another read', async () => {
  const f = fixture(); await f.model.refresh();
  const pending = deferred(); f.handle(() => pending.promise);
  const result = f.model.read('Study@home'); f.model.close();
  pending.resolve(eq());
  assert.equal(await result, false);
  assert.equal(f.model.state.open, false);
  assert.equal(f.model.state.loading, false);
  assert.equal(f.model.state.current, null);
  assert.equal(await f.model.read('Study@home'), false);
  assert.equal(f.calls.length, 2);
});

test('closing during either preflight cancels the intent before any speaker mutation', async () => {
  for (const stage of ['zones', 'eqRead']) {
    const f = fixture(), m = await f.ready(), pending = deferred();
    f.handle((route) => route === stage ? pending.promise : zones());
    m.choose('bass', 3);
    const applying = m.apply(); await tick(); m.close();
    pending.resolve(stage === 'zones' ? zones() : eq());
    assert.equal(await applying, false);
    assert.equal(f.writes().length, 0);
    assert.equal(m.state.writing, false);
    assert.equal(m.state.current, null);
  }
});

test('double submit and all competing edits/reads are blocked until the one write settles', async () => {
  const f = fixture(), m = await f.ready(), pending = deferred();
  f.handle((route) => route === 'zones' ? zones() : route === 'eqRead' ? eq() : pending.promise);
  m.choose('bass', 3);
  const first = m.apply(); await tick();
  assert.equal(f.writes().length, 1);
  assert.equal(await m.apply(), false);
  assert.equal(await m.read(), false);
  assert.equal(await m.refresh(), false);
  assert.equal(m.choose('treble', 2), false);
  assert.equal(m.flat(), false);
  assert.equal(m.discard(), false);
  assert.equal(f.writes().length, 1);
  pending.resolve({ done: 'Changed.', eq: eq('Study', { bass: 3 }) });
  assert.equal(await first, true);
  assert.equal(m.state.writing, false);
});

test('a lost or rejected reply clears the draft, preserves the hint and never repeats the write', async () => {
  for (const code of ['REMOTE_OUTCOME_UNKNOWN', 'UPNP_FAULT', 'POLICY_DENIED']) {
    const f = fixture(), m = await f.ready();
    f.handle((route) => {
      if (route === 'zones') return zones();
      if (route === 'eqRead') return eq();
      throw Object.assign(new Error('Could not confirm.'), { code, hint: 'Check the speaker.' });
    });
    m.choose('bass', 3);
    assert.equal(await m.apply(), false);
    assert.equal(f.writes().length, 1);
    assert.equal(m.state.current, null);
    assert.equal(m.state.draft, null);
    assert.equal(m.state.writing, false);
    assert.match(m.state.message, /Check the speaker/);
    assert.match(m.state.message, /partial or uncertain/);
    assert.match(m.state.message, /not repeated/);
    assert.equal(await m.apply(), false);
    assert.equal(f.calls.length, 5, 'no automatic post-write read can disguise uncertainty');
  }
});

test('an incomplete or mismatched success is uncertain, never an acknowledgement', async () => {
  const valid = { done: 'Changed.', eq: eq('Study', { bass: 3 }) };
  for (const response of [null, {}, { ...valid, done: '' }, { ...valid, done: 1 },
    { ...valid, eq: eq('Kitchen', { bass: 3 }) }, { ...valid, eq: eq() },
    { ...valid, eq: eq('Study', { bass: 3, loudness: false }) },
    { ...valid, eq: eq('Study', { bass: 3, treble: 11 }) }]) {
    const f = fixture(), m = await f.ready();
    f.handle((route) => route === 'zones' ? zones() : route === 'eqRead' ? eq() : response);
    m.choose('bass', 3);
    assert.equal(await m.apply(), false, JSON.stringify(response));
    assert.equal(f.writes().length, 1);
    assert.equal(m.state.current, null);
    assert.equal(m.canApply(), false);
    assert.match(m.state.message, /uncertain/);
  }
});

test('an error before dispatch reports no mutation and retains its useful hint', async () => {
  const f = fixture(), m = await f.ready();
  f.handle(() => { throw Object.assign(new Error('Discovery unavailable.'), { hint: 'Check the network.' }); });
  m.choose('bass', 4);
  assert.equal(await m.apply(), false);
  assert.equal(f.writes().length, 0);
  assert.match(m.state.message, /Check the network/);
  assert.match(m.state.message, /No sound change was sent/);
});

test('closing and reopening an in-flight write neither cancels the speaker nor permits another write', async () => {
  const f = fixture(), m = await f.ready(), pending = deferred();
  f.handle((route) => route === 'zones' ? zones() : route === 'eqRead' ? eq() : pending.promise);
  m.choose('bass', 4);
  const applying = m.apply(); await tick(); m.close();
  assert.equal(await m.refresh(), false);
  assert.equal(m.state.open, true);
  assert.equal(m.state.current, null);
  assert.equal(await m.apply(), false);
  pending.resolve({ done: 'Changed.', eq: eq('Study', { bass: 4 }) });
  assert.equal(await applying, true, 'the old request was acknowledged, but cannot repaint a reopened editor');
  assert.equal(m.state.current, null);
  assert.equal(m.state.draft, null);
  assert.equal(m.state.writing, false);
  assert.equal(f.writes().length, 1);
  assert.equal(await m.read(), true, 'an explicit fresh read is available');
});

test('disposal suppresses later publications and cannot be reversed by a read or acknowledgement', async () => {
  for (const stage of ['eqRead', 'eqWrite']) {
    const f = fixture(), m = await f.ready(), pending = deferred();
    f.handle((route) => route === stage ? pending.promise : route === 'zones' ? zones() : eq());
    m.choose('bass', 4);
    const action = stage === 'eqRead' ? m.read() : m.apply();
    await tick(); m.dispose();
    const count = f.publications();
    pending.resolve(stage === 'eqRead' ? eq() : { done: 'Changed.', eq: eq('Study', { bass: 4 }) });
    await action;
    assert.equal(f.publications(), count);
    assert.equal(m.state.current, null);
    assert.equal(await m.refresh(), false);
    assert.equal(await m.read(), false);
    assert.equal(await m.apply(), false);
  }
});

test('slider drafts can update without replacing the active DOM control on every input event', async () => {
  const f = fixture(), m = await f.ready(), count = f.publications();
  for (const value of [3, 4, 5]) assert.equal(m.choose('bass', value, false), true);
  assert.equal(f.publications(), count);
  assert.equal(m.state.draft.bass, 5);
  assert.equal(m.canApply(), true);
  assert.equal(f.writes().length, 0);
});

test('no stale target survives a refresh that removes it', async () => {
  const f = fixture(), m = await f.ready();
  f.inventory([zones()[1]]);
  assert.equal(await m.refresh(), true);
  assert.equal(m.state.target, '');
  assert.equal(m.state.current, null);
  assert.equal(await m.read('Study@home'), false);
});

test('route definitions use the existing public EQ endpoints', () => {
  assert.deepEqual(Equalizer.routes, { eqRead: ['GET', '/zones/{room}/eq'], eqWrite: ['POST', '/eq'] });
});

test('the delivered module embeds no addresses or credentials and uses text-only metadata rendering', () => {
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/equalizer.js'), 'utf8');
  for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
    assert.equal(source.toLowerCase().includes(secret), false, secret);
  }
  assert.doesNotMatch(source, /innerHTML|outerHTML|insertAdjacentHTML|eval\(/);
});
