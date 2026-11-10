// Exercise the shipped source selectors; all devices and playback are synthetic.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const clone = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
const input = (household = 'S2') => ({ player: 'RINCON_FIXTURE_INPUT_' + household, room: 'Record player', name: 'Turntable', icon: '1' });

async function fixture(kind) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'playing' }));
  const rows = new Map(zones.map(zone => [Core.key(zone), { zone, transport_state: 'playing', volume: 20, mute: false,
    track: { uri: 'fixture:current', title: 'Current song', queue_position: 3, position_secs: 20, duration_secs: 300 } }]));
  const catalogs = new Map(zones.map(zone => [Core.key(zone), { sources: [input(zone.household)],
    unavailable: [{ player: 'RINCON_FIXTURE_OFFLINE_' + zone.household, room: 'Other input', detail: 'The player did not answer.' }] }]));
  const calls = []; let source, controller, reader = null, writer = null, executions = 0;
  const read = url => {
    if (url === '/zones') return clone(zones);
    if (url === '/schedules') return [];
    if (url.startsWith('/line-in?')) return clone(catalogs.get(new URLSearchParams(url.split('?')[1]).get('zone')));
    if (url.endsWith('/state')) return clone(rows.get(decodeURIComponent(url.split('/')[2])));
    throw new Error('Unexpected read: ' + url);
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body: clone(body) });
    if (body !== undefined) {
      assert.equal(url, '/line-in/play', 'line-in never uses the raw URI or queue endpoints');
      if (writer) return writer(url, body);
      return { done: 'Playing line-in', changed: true, notes: [{ detail: 'Volume capped.' }] };
    }
    return reader ? reader(url, options, read) : read(url);
  };
  if (kind === 'phone') {
    source = Phone.model({ call(name, args = {}) {
      const route = name === 'zones' ? '/zones' : name === 'state' ? '/zones/' + encodeURIComponent(args.params.room) + '/state'
        : Phone.routes[name]?.[1];
      assert.ok(route, 'known source route ' + name);
      return raw(route + (args.query ? '?' + new URLSearchParams(args.query) : ''), args.body);
    }, execute: async (_, work) => { executions++; return work(); } });
    await source.refresh(); source.edit('target', 'Den@S2');
  } else {
    controller = Core.model({ call: raw, now: () => 1000, changed: () => source?.observe() });
    source = Dashboard.model({ controller, call: raw, now: () => 1000 });
    assert.equal(await controller.refresh(), true); controller.connection(true); controller.select('Den@S2'); source.open();
  }
  assert.equal(source.edit('sourceType', 'line_in'), true);
  return { source, controller, zones, rows, catalogs, calls, executions: () => executions,
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, writes: () => calls.filter(call => call.body !== undefined),
    reads: () => calls.filter(call => call.url.startsWith('/line-in?')),
    select() { if (kind === 'phone') source.edit('target', 'Den@S1'); else controller.select('Den@S1'); },
    async choose() { assert.equal(await source.loadLineInSources(), true); assert.equal(source.edit('lineIn', input().player), true); },
    async review() { assert.ok(await source.review()); return source.state.pending; },
    confirm(pending = source.state.pending) { return kind === 'phone' ? source.confirm(pending) : source.apply(pending); },
  };
}

test('line-in catalogs retain stable player identities and report partial availability', () => {
  const catalog = Phone.lineInSources({ sources: [input()], unavailable: [{ player: 'RINCON_FIXTURE_UNAVAILABLE', room: 'Other room', detail: 'Try again.' }] });
  assert.ok(Object.isFrozen(catalog)); assert.ok(Object.isFrozen(catalog.sources)); assert.ok(Object.isFrozen(catalog.sources[0]));
  assert.deepEqual(catalog.sources[0], input());
  const choice = Phone.prepareLineIn(input().player, catalog.sources, 'play');
  assert.deepEqual(Phone.commandBody(choice, 'Den@S2'), { zone: 'Den@S2', source: input().player });
  assert.equal(choice.item.container, false); assert.equal(choice.title, 'Turntable · Record player');
  assert.match(Phone.lineInHelp({ ...catalog, lineInSources: catalog.sources, lineInUnavailable: catalog.unavailable, lineInStatus: 'ready' }), /Other room: Try again/);
  for (const mode of ['next', 'end', 'unknown']) assert.throws(() => Phone.prepareLineIn(input().player, catalog.sources, mode));
  assert.throws(() => Phone.commandBody({ ...choice, mode: 'next' }, 'Den@S2'));
  assert.throws(() => Phone.prepareLineIn('RINCON_UNOBSERVED', catalog.sources, 'play'));
  assert.throws(() => Phone.source('x-rincon-stream:' + input().player), 'the raw URI allowlist is unchanged');
});

test('missing, contradictory and malformed line-in replies cannot become selections', () => {
  for (const value of [null, {}, { sources: [input()] }, { sources: [], unavailable: null },
    { sources: [input(), input()], unavailable: [] },
    { sources: [input()], unavailable: [{ ...input(), detail: 'Contradictory status' }] },
    { sources: [{ ...input(), player: '' }], unavailable: [] },
    { sources: [{ ...input(), room: null }], unavailable: [] },
    { sources: [{ ...input(), name: null }], unavailable: [] },
    { sources: [{ ...input(), icon: null }], unavailable: [] },
    { sources: [{ ...input(), player: 'bad\nidentity' }], unavailable: [] },
    { sources: [], unavailable: [{ player: 'fixture:offline', room: 'Room', detail: '' }] }]) {
    assert.throws(() => Phone.lineInSources(value));
  }
});

for (const kind of ['phone', 'dashboard']) {
  test(kind + ': input discovery is explicit, read-only and scoped to the destination household', async () => {
    const f = await fixture(kind);
    assert.equal(f.reads().length, 0); assert.equal(f.writes().length, 0);
    assert.equal(f.source.edit('lineIn', input().player), false);
    assert.ok(!await f.source.review()); assert.equal(f.reads().length, 0);
    await f.choose(); assert.equal(f.reads().length, 1);
    assert.equal(new URL(f.reads()[0].url, 'http://fixture').searchParams.get('zone'), 'Den@S2');
    assert.equal(f.source.state.lineInSources.length, 1); assert.equal(f.source.state.lineInUnavailable.length, 1);
    assert.equal(f.source.state.pending, null); assert.equal(f.writes().length, 0);
    assert.match(Phone.lineInHelp(f.source.state), /signal state are not reported/);
    assert.equal(f.source.edit('mode', 'next'), false); assert.equal(f.source.edit('mode', 'end'), false);
  });

  test(kind + ': review and confirmation re-read the exact input, then send one coordinated play', async () => {
    const f = await fixture(kind); await f.choose(); const pending = await f.review();
    assert.equal(f.reads().length, 2); assert.equal(f.writes().length, 0);
    assert.equal(pending.item.kind, 'line_in'); assert.equal(pending.mode, 'play'); assert.equal(pending.before.queue, undefined);
    assert.deepEqual(pending.body, { zone: 'Den@S2', source: input().player });
    assert.equal(await f.confirm(pending), true); assert.equal(await f.confirm(pending), false);
    assert.equal(f.reads().length, 3);
    assert.deepEqual(f.writes(), [{ url: '/line-in/play', body: { zone: 'Den@S2', source: input().player } }]);
    assert.equal(f.source.state.result.status, 'accepted'); assert.match(f.source.state.result.detail, /Volume capped/);
    if (kind === 'phone') assert.equal(f.executions(), 1);
    else assert.match(Dashboard.effect(pending), /analog input from Record player/);
  });

  test(kind + ': switching from a queue action to line-in resets the action to Play now', async () => {
    const f = await fixture(kind);
    f.source.edit('sourceType', 'link'); assert.equal(f.source.edit('mode', 'end'), true);
    assert.equal(f.source.edit('sourceType', 'line_in'), true); assert.equal(f.source.state.mode, 'play');
    await f.choose(); assert.equal((await f.review()).mode, 'play');
  });

  test(kind + ': same-name inputs in another household cannot inherit a selection or review', async () => {
    const f = await fixture(kind); await f.choose(); const pending = await f.review(); f.select();
    assert.equal(f.source.state.lineIn, ''); assert.equal(f.source.state.lineInTarget, '');
    assert.equal(f.source.state.pending, null); assert.equal(await f.confirm(pending), false);
    assert.equal(await f.source.loadLineInSources(), true);
    assert.equal(f.source.edit('lineIn', input().player), false); assert.equal(f.source.edit('lineIn', input('S1').player), true);
    assert.deepEqual((await f.review()).body, { zone: 'Den@S1', source: input('S1').player });
    assert.equal(f.writes().length, 0);
  });

  for (const phase of ['review', 'confirm']) for (const change of ['identity', 'name', 'room', 'icon', 'unavailable']) {
    test(kind + ': changed input ' + change + ' blocks ' + phase + ' without a write', async () => {
      const f = await fixture(kind); await f.choose();
      const pending = phase === 'confirm' ? await f.review() : null;
      const catalog = f.catalogs.get('Den@S2');
      if (change === 'identity') catalog.sources[0].player = 'RINCON_REPLACEMENT';
      else if (change === 'unavailable') { catalog.unavailable.push({ player: input().player, room: input().room, detail: 'Offline' }); catalog.sources = []; }
      else catalog.sources[0][change] = 'Changed';
      assert.ok(!await (phase === 'confirm' ? f.confirm(pending) : f.source.review()));
      assert.equal(f.writes().length, 0); assert.match(f.source.state.message || f.source.state.result?.detail || '', /line-in source changed|unavailable/);
    });
  }

  test(kind + ': playback changes and source read errors retire a review before sending', async () => {
    for (const change of ['playback', 'input']) {
      const f = await fixture(kind); await f.choose(); const pending = await f.review();
      if (change === 'playback') f.rows.get('Den@S2').track.uri = 'fixture:other';
      else f.read((url, options, read) => { if (url.startsWith('/line-in?')) throw new Error('Input read failed'); return read(url); });
      assert.equal(await f.confirm(pending), false); assert.equal(f.writes().length, 0); assert.equal(f.source.state.pending, null);
    }
  });

  test(kind + ': a lost playback reply stays uncertain and cannot replay the consumed review', async () => {
    const f = await fixture(kind); await f.choose(); const pending = await f.review();
    f.write(() => { throw new Error('Reply lost'); });
    assert.equal(await f.confirm(pending), false); assert.equal(await f.confirm(pending), false);
    assert.equal(f.writes().length, 1); assert.equal(f.source.state.result.status, 'uncertain');
    assert.match(f.source.state.result.detail, /not repeated|not replayed|uncertain|reply/i);
  });

  for (const action of ['household', 'type', 'close', 'dispose']) {
    test(kind + ': a late input reply cannot revive a selection after ' + action, async () => {
      const f = await fixture(kind), gate = deferred();
      f.read(async (url, options, read) => { const value = read(url); if (url.startsWith('/line-in?')) await gate.promise; return value; });
      const work = f.source.loadLineInSources(); assert.equal(await f.source.loadLineInSources(), false);
      if (action === 'household') f.select();
      else if (action === 'type') f.source.edit('sourceType', 'link');
      else f.source[action]();
      gate.resolve(); assert.equal(await work, false); assert.equal(f.source.state.lineInSources.length, 0);
      assert.equal(f.source.state.lineIn, ''); assert.equal(f.source.state.pending, null); assert.equal(f.writes().length, 0);
    });
  }

  test(kind + ': an empty household and unavailable players remain distinct read results', async () => {
    const f = await fixture(kind); f.catalogs.set('Den@S2', { sources: [], unavailable: [] });
    assert.equal(await f.source.loadLineInSources(), true); assert.match(Phone.lineInHelp(f.source.state), /No line-in inputs are advertised/);
    f.catalogs.get('Den@S2').unavailable.push({ player: 'RINCON_FIXTURE_OFFLINE', room: 'Input room', detail: 'Unavailable' });
    assert.equal(await f.source.loadLineInSources(), true); assert.match(Phone.lineInHelp(f.source.state), /No line-in source could be read/);
    assert.ok(!await f.source.review()); assert.equal(f.writes().length, 0);
  });
}
