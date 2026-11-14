'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { T, fixture, target, defer, until } = require('./dj-targets-fixture.cjs');

for (const action of ['start', 'steer', 'clear', 'skip', 'stop']) {
  test(`${action} reviews all qualified groups and dispatches one revision-bound DJ request`, async () => {
    const h = fixture(), pending = await h.review(action);
    assert.equal(pending.before.groups.length, 3); assert.equal(pending.before.view.rooms.length, 4);
    assert.deepEqual(pending.before.groups.map(group => group.target), ['Den@S1', 'Den@S2', 'Kitchen@S2']);
    assert.equal(pending.before.groups[0].members.length, 3, 'the alias also affects its unselected grouped room');
    assert.equal(h.writes().length, 0);
    assert.equal(await h.control.confirm(pending), true, h.control.state.result?.detail);
    assert.equal(h.executions, 1); assert.equal(h.held.length, 3); assert.equal(h.completed.length, 1);
    assert.deepEqual(h.writes().map(row => row.args.body), [{ zone: target, action: action === 'clear' ? 'steer' : action,
      ...(action === 'clear' ? { clear: true } : action === 'steer' ? { mood: 'focus', constraints: {} } : {}),
      expected_revision: pending.before.view.revision }]);
    assert.equal(await h.control.confirm(pending), false); assert.equal(h.writes().length, 1);
    assert.match(h.control.state.result.done, /Den@S1.*Den@S2.*Kitchen@S2/);
  });
}

test('directed starts preserve the exact full-filter direction and duration without inheriting one group’s session', async () => {
  const h = fixture(); await h.open('start', true);
  const fields = { include_artists: 'Earth, Wind & Fire\nBjörk', exclude_artists: 'Other performer', include_genres: 'jazz\nsoul',
    exclude_genres: 'metal', include_composers: 'Bach', exclude_composers: 'Other composer', periods: 'baroque\nlate romantic',
    exclude_periods: 'modern', decades: '1960\n1970', exclude_decades: '1980', include_keywords: 'live', exclude_keywords: 'remix',
    min_work_minutes: '2', max_work_minutes: '45', energy_bias: '-1', allow_long_works: true, strict: true, minutes: '90' };
  for (const [field, value] of Object.entries(fields)) assert.equal(h.control.editDirection(field, value), true);
  const p = await h.control.review(); assert.ok(p, h.control.state.message);
  assert.equal(Object.isFrozen(p.body.constraints.include_artists), true);
  assert.equal(p.body.for_secs, 5400);
  assert.deepEqual(p.body.constraints, { include_artists: ['Earth, Wind & Fire', 'Björk'], exclude_artists: ['Other performer'],
    include_genres: ['jazz', 'soul'], exclude_genres: ['metal'], include_composers: ['Bach'], exclude_composers: ['Other composer'],
    periods: ['baroque', 'late_romantic'], exclude_periods: ['modern'], decades: [1960, 1970], exclude_decades: [1980],
    include_keywords: ['live'], exclude_keywords: ['remix'], min_work_minutes: 2, max_work_minutes: 45,
    energy_bias: -1, allow_long_works: true, strict: true });
  assert.equal(await h.control.confirm(p), true);
  assert.deepEqual(h.writes()[0].args.body, p.body);
});

test('clear and undirected start never copy leftover direction fields or restart a prior duration', async () => {
  for (const action of ['clear', 'start']) {
    const h = fixture(); await h.open(action);
    h.control.edit('mood', 'focus'); h.control.editDirection('include_genres', 'jazz'); h.control.editDirection('minutes', '90');
    const p = await h.control.review(); assert.ok(p); assert.equal(p.body.constraints, undefined); assert.equal(p.body.mood, undefined);
    assert.equal(p.body.for_secs, undefined); assert.equal(await h.control.confirm(p), true);
  }
});

test('malformed constraints, empty directed sessions and unsupported revisions fail before any write', async () => {
  const h = fixture(); await h.open('steer'); h.control.edit('mood', '');
  assert.equal(await h.control.review(), null); assert.match(h.control.state.message, /Choose a mood or filter/);
  h.control.editDirection('decades', '1963'); assert.equal(await h.control.review(), null);
  for (const revision of [undefined, '', 'ABC'.repeat(22), 'z'.repeat(64), 'a'.repeat(63)]) {
    const f = fixture(); await f.open(); f.custom = (name, args) => name === 'djTargetsResolve' ? { ...f.resolution(args.query.target), revision } : undefined;
    assert.equal(await f.control.review(), null); assert.match(f.control.state.message, /Update the daemon/); assert.equal(f.writes().length, 0);
  }
  assert.equal(h.writes().length, 0);
});

test('readiness reads never silently drop an unreadable group or conflate identical room names across households', async () => {
  const h = fixture(); await h.open(); h.statuses.get('Den@S2').zone = 'Wrong room';
  assert.equal(await h.control.review(), null); assert.match(h.control.state.message, /Den@S2/); assert.equal(h.writes().length, 0);
  const f = fixture(); await f.open(); f.custom = (name, args) => {
    if (name !== 'djTargetsResolve') return;
    const view = f.resolution(args.query.target); view.groups.push(view.groups[0]); return view;
  };
  assert.equal(await f.control.review(), null); assert.match(f.control.state.message, /incomplete or contradictory/);
});

for (const phase of ['review', 'confirmation']) test(`physical player or address changes during ${phase} invalidate every target`, async () => {
  for (const mutate of [h => { h.addressRevision++; }, h => { h.groups[0].members[2].player = 'replacement-patio'; }]) {
    const h = fixture();
    if (phase === 'review') {
      let reads = 0; h.custom = name => { if (name === 'djTargetsResolve' && ++reads === 2) mutate(h); };
      await h.open(); assert.equal(await h.control.review(), null);
    } else { const p = await h.review(); mutate(h); assert.equal(await h.control.confirm(p), false); }
    assert.equal(h.writes().length, 0);
  }
});

test('an alias expansion cannot include a newly selected group even when all old groups survive', async () => {
  const h = fixture(), p = await h.review();
  const room = { household: 'S2', room: 'Bedroom', player: 'S2-Bedroom', coordinator: 'S2-Bedroom' };
  h.selected.push(room); h.groups.push({ household: 'S2', coordinator: room.player, coordinator_room: room.room, members: [room] });
  const zone = h.zones().at(-1); h.states.set('Bedroom@S2', { ...h.states.get('Kitchen@S2'), zone });
  h.statuses.set('Bedroom@S2', { ...h.statuses.get('Kitchen@S2'), zone: 'Bedroom' });
  h.queues.set('Bedroom@S2', { ...h.queues.get('Kitchen@S2'), zone: 'Bedroom' });
  assert.equal(await h.control.confirm(p), false); assert.equal(h.writes().length, 0);
});

test('new source, queue, session, running state or mood definition consumes the review before dispatch', async () => {
  for (const mutate of [h => { h.states.get('Den@S1').track.uri = 'fixture:new-source'; },
    h => { h.queues.get('Den@S2').revision += '-changed'; }, h => { h.statuses.get('Kitchen@S2').running = true; },
    h => { h.statuses.get('Den@S1').steering.added = { include_genres: ['soul'] }; },
    h => { h.moods.moods[0].constraints = { strict: true }; }]) {
    const h = fixture(), p = await h.review('start', true); mutate(h);
    assert.equal(await h.control.confirm(p), false); assert.equal(h.writes().length, 0);
    assert.equal(h.control.state.result.status, 'not_sent');
  }
});

test('steering tolerates track progress and decreasing expiry but detects changed session semantics', async () => {
  const h = fixture(), p = await h.review('steer');
  h.statuses.get('Den@S1').steering.expires_in_secs -= 10;
  h.states.get('Den@S1').track.uri = 'fixture:next-track';
  assert.equal(await h.control.confirm(p), true);
  const f = fixture(), prior = await f.review('steer');
  f.statuses.get('Den@S1').steering.session_mood = null;
  assert.equal(await f.control.confirm(prior), false); assert.equal(f.writes().length, 0);
});

test('a group changing while its command waits for a shared lane cannot receive the old command', async () => {
  const h = fixture(), p = await h.review(), gate = defer(); let held = false;
  h.execute = async (_, work) => { held = true; await gate.promise; return work(); };
  const pending = h.control.confirm(p); await until(() => held); h.addressRevision++; gate.resolve();
  assert.equal(await pending, false); assert.equal(h.writes().length, 0);
  assert.equal(await h.control.confirm(p), false);
});

test('closed and expired reviews suppress unsent commands, including delayed confirmation reads', async () => {
  for (const retire of [h => h.control.close(), h => { h.time += T.MAX_AGE; }]) {
    const h = fixture(), p = await h.review(), wait = defer();
    h.custom = (name, args) => name === 'djTargetsState' && args.params.room === 'Den@S1' ? wait.promise : undefined;
    const pending = h.control.confirm(p); await until(() => h.calls.filter(row => row.name === 'djTargetsState').length > 3);
    retire(h); wait.resolve(h.states.get('Den@S1'));
    assert.equal(await pending, false); assert.equal(h.writes().length, 0);
  }
});

test('partial receipts preserve every failure and require an explicit read, never replaying completed groups', async () => {
  const h = fixture(), p = await h.review('skip');
  h.reply = () => ({ done: 'Den@S1: skipped current work; Den@S2: skipped current work', changed: true,
    notes: [{ code: 'SKIPPED', detail: 'Kitchen@S2: DJ_NOT_RUNNING: there is no active feed' },
      { code: 'VOLUME_CAPPED', detail: 'Den@S1: limited by household policy' }] });
  assert.equal(await h.control.confirm(p), true); assert.equal(h.control.state.result.partial, true);
  assert.match(h.control.state.result.detail, /Kitchen@S2.*DJ_NOT_RUNNING/); assert.equal(h.control.state.result.notes.length, 2);
  assert.equal(await h.control.review(), null); assert.equal(await h.control.confirm(p), false);
  h.selected = [h.groups[2].members[0]]; // The old alias no longer describes the old request.
  assert.equal(await h.control.observeResult(), true, h.control.state.message);
  assert.deepEqual(h.control.state.observation.entries.map(entry => entry.group.target), ['Den@S1', 'Den@S2', 'Kitchen@S2']);
  assert.equal(h.control.state.result.partial, true); assert.equal(h.control.state.result.status, 'observed');
  assert.equal(h.writes().length, 1);
});

test('uncertain responses survive close and edits until affected-player observation, with no command retry', async () => {
  for (const reply of [() => { throw new Error('Reply lost after applying'); }, () => null,
    () => ({ done: 'Missing status' }), () => ({ done: 'Incomplete notes', changed: true, notes: [{ detail: 'Missing code' }] })]) {
    const h = fixture(), p = await h.review(); h.reply = reply;
    assert.equal(await h.control.confirm(p), false); assert.equal(h.control.state.result.status, 'uncertain');
    h.control.close(); h.control.open(); h.control.edit('input', 'all@S2');
    assert.equal(await h.control.review(), null); assert.equal(h.writes().length, 1);
    assert.equal(await h.control.observeResult(), true); assert.equal(h.control.state.recovery, null);
    assert.equal(h.control.state.result.status, 'observed'); assert.match(h.control.state.result.detail, /uncertain/);
    assert.equal(h.writes().length, 1);
  }
});

test('explicit observation follows original players across regrouping and retains unreachable-group failures', async () => {
  const h = fixture(), p = await h.review('stop'); h.reply = () => { throw new Error('Lost reply'); };
  await h.control.confirm(p);
  const lead = h.groups[0]; lead.coordinator_room = 'Side'; lead.coordinator = 'S1-Side';
  for (const room of lead.members) room.coordinator = lead.coordinator;
  const zone = h.zones()[0]; h.states.set('Side@S1', { ...h.states.get('Den@S1'), zone });
  h.statuses.set('Side@S1', { ...h.statuses.get('Den@S1'), zone: 'Side' });
  let broken = true;
  h.custom = (name, args) => { if (broken && name === 'djTargetsStatus' && args.params.room === 'Den@S2') throw new Error('Speaker unavailable'); };
  assert.equal(await h.control.observeResult(), false); assert.ok(h.control.state.recovery);
  assert.equal(h.control.state.observation.entries.length, 3);
  assert.ok(h.control.state.observation.entries.find(entry => entry.group.target === 'Den@S2').error);
  assert.ok(h.control.state.observation.entries.some(entry => entry.group.target === 'Side@S1' && entry.status));
  broken = false; assert.equal(await h.control.observeResult(), true);
  assert.equal(h.writes().length, 1);
});

test('lost writes stay single-flight through close/reopen and retain a late confirmed result', async () => {
  const h = fixture(), p = await h.review(), reply = defer(); h.reply = () => reply.promise;
  const result = h.control.confirm(p); await until(() => h.writes().length === 1);
  h.control.close(); h.control.open(); assert.equal(h.control.state.writing, true);
  assert.equal(h.control.edit('action', 'stop'), false); assert.equal(await h.control.confirm(p), false);
  reply.resolve({ done: 'All reviewed groups accepted', changed: true, notes: [] });
  assert.equal(await result, true); assert.equal(h.control.state.result.status, 'accepted'); assert.equal(h.writes().length, 1);
});

test('catalog suggestions are lazy and a mood-catalog error never blocks skip or stop', async () => {
  const h = fixture(); assert.equal(h.calls.length, 0); await h.open('stop');
  h.custom = name => { if (name === 'djTargetsMoods') throw new Error('Mood configuration unavailable'); };
  assert.equal(await h.control.refresh(), false); assert.ok(h.control.state.suggestions.some(item => item.query === 'all@S1'));
  assert.equal(h.control.state.input, target); const p = await h.control.review(); assert.ok(p, h.control.state.message);
  assert.equal(await h.control.confirm(p), true);
});

test('playback events for affected groups retire source-changing reviews but preserve steering drafts', async () => {
  const h = fixture(), p = await h.review(); const state = structuredClone(h.states.get('Den@S2')); state.track.uri = 'fixture:event';
  h.control.observePlayback(state); assert.equal(h.control.state.pending, null); assert.equal(await h.control.confirm(p), false);
  const f = fixture(), steer = await f.review('steer'); f.control.observePlayback(state);
  assert.equal(f.control.state.pending, steer);
  f.control.invalidate({ household: 'Other', members: ['Elsewhere'] }); assert.equal(f.control.state.pending, steer);
  f.control.invalidate({ household: 'S2', members: ['Den'] }); assert.equal(f.control.state.pending, null);
});

test('dashboard batch owns the existing whole-house lane and refreshes every affected group', async () => {
  const h = fixture({ dashboard: true }), p = await h.review('start', true), reply = defer(); h.reply = () => reply.promise;
  const result = h.control.confirm(p); await until(() => h.writes().length === 1);
  assert.equal(h.controller.state.busy, true); assert.equal(await h.controller.command('resume'), false);
  assert.equal(h.control.edit('input', 'all@S1'), false);
  reply.resolve({ done: 'All three DJ groups accepted', changed: true, notes: [] });
  assert.equal(await result, true); assert.equal(h.controller.state.busy, false); assert.equal(h.writes().length, 1);
  assert.ok(h.controller.state.items.every(item => item.fresh));
});

test('dashboard unknown outcome needs explicit affected-room observation even after passive refresh', async () => {
  const h = fixture({ dashboard: true }), p = await h.review('skip'); h.reply = () => { throw new Error('Reply lost'); };
  assert.equal(await h.control.confirm(p), false); assert.equal(h.controller.state.uncertain, true);
  await h.controller.refresh(false); assert.equal(h.controller.state.uncertain, true); assert.equal(await h.control.review(), null);
  assert.equal(await h.control.observeResult(), true, h.control.state.message); assert.equal(h.controller.state.uncertain, false);
  assert.equal(h.control.state.recovery, null); assert.equal(h.writes().length, 1);
});

test('dashboard stale state in any reviewed group retires the batch while leaving the selected group alone', async () => {
  const h = fixture({ dashboard: true }), p = await h.review('stop');
  h.controller.invalidate(['Kitchen@S2']);
  assert.equal(h.controller.selected().fresh, true); assert.equal(h.control.state.pending, null);
  assert.equal(await h.control.confirm(p), false); assert.equal(h.writes().length, 0);
});


test('missing notes are valid but explicit malformed notes retain the DJ recovery barrier', async () => {
  for (const notes of [undefined, [], null, {}, 'SKIPPED']) {
    const h = fixture(), pending = await h.review();
    h.reply = () => ({ done: 'The selected DJ request returned', changed: true,
      ...(notes === undefined ? {} : { notes }) });
    const valid = notes === undefined || Array.isArray(notes);
    assert.equal(await h.control.confirm(pending), valid);
    assert.equal(h.writes().length, 1);
    assert.equal(h.control.state.result.status, valid ? 'accepted' : 'uncertain');
    if (valid) assert.equal(h.control.state.recovery, null);
    else {
      assert.ok(h.control.state.recovery);
      assert.equal(await h.control.review(), null);
      assert.equal(await h.control.confirm(pending), false);
      assert.equal(h.writes().length, 1);
      assert.equal(await h.control.observeResult(), true);
      assert.equal(h.writes().length, 1);
    }
  }
});
