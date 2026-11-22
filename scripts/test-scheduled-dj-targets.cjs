'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const S = require('../crates/fsonos-api/src/remote/schedules.js');
const R = require('../crates/fsonos-api/src/remote/dashboard-routines.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const targets = require('./schedule-targets-fixture.cjs');
const copy = value => structuredClone(value);
const defer = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
const row = (extra = {}) => ({ id: 7, revision: 'saved-v1', when: 'weekdays 07:30', action: 'dj_start',
  zone: 'together', mood: 'calm', direction: { constraints: { include_genres: ['jazz'] }, for_secs: 900 },
  summary: 'Saved DJ routine', creator: 'original creator', enabled: false, next: null, last_fired: null, ...extra });

function fixture(surface, initial = [], action = 'dj_start') {
  const groups = [
    { household: 'S1', coordinator_room: 'Lead', members: ['Lead', 'Member', 'Guest'] },
    { household: 'S2', coordinator_room: 'Lead', members: ['Lead'] },
    { household: 'S2', coordinator_room: 'Kitchen', members: ['Kitchen'] },
  ];
  const aliases = { together: ['Lead@S1', 'Member@S1', 'Lead@S2'] };
  const moods = { moods: [{ name: 'calm', constraints: { include_genres: ['jazz'] } }] };
  let rows = copy(initial), physical = '', intercept = null, rejectWrite = null, loseReply = false, control;
  const calls = [];
  const resolve = query => targets.resolve(groups, query, aliases, physical);
  const call = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    if (body === undefined) {
      if (intercept) { const value = intercept(url); if (value !== undefined) return value; }
      if (url === '/schedules') return copy(rows);
      if (url === '/rooms') return targets.rooms(groups, aliases);
      if (url === '/zones') return copy(groups);
      if (url === '/dj/moods') return copy(moods);
      if (url.startsWith('/rooms/resolve?')) return resolve(new URL(url, 'http://fixture').searchParams.get('target'));
      throw new Error('Unexpected read: ' + url);
    }
    assert.ok(['/schedules', '/schedules/update'].includes(url), 'routine editing never dispatches a DJ or speaker command');
    if (rejectWrite) throw rejectWrite;
    const original = url === '/schedules/update' ? rows.find(saved => saved.id === body.id) : null;
    if (original) assert.equal(body.expected_revision, original.revision);
    const definition = original ? body.schedule : body;
    const saved = row({ ...copy(definition), mood: definition.mood ?? null, direction: definition.direction ?? null,
      fade_secs: definition.fade_secs ?? null, id: original?.id ?? 8,
      revision: original ? original.revision + '.next' : 'new-v1', creator: original?.creator ?? 'current caller',
      enabled: original?.enabled ?? true, next: original && !original.enabled ? null : '2030-06-18T07:30:00Z' });
    rows = original ? rows.map(item => item.id === original.id ? saved : item) : [...rows, saved];
    if (loseReply) throw new Error('The saved routine reply was lost');
    return copy(saved);
  };
  const named = (name, args = {}) => call(S.routes[name][1]
    + (args.query ? '?' + new URLSearchParams(args.query) : ''), args.body, args);
  const controller = surface === 'dashboard' ? C.model({ call, changed: () => control?.observe() }) : null;
  control = controller ? R.model({ call, controller }) : S.model({ call: named });
  return { control, groups, aliases, moods, calls, resolve,
    physical(value) { physical = value; }, intercept(value) { intercept = value; }, reject(value) { rejectWrite = value; },
    lose(value = true) { loseReply = value; },
    rows: () => copy(rows), writes: () => calls.filter(call => call.body !== undefined),
    refresh() { return controller ? control.read(true) : control.refresh(true); },
    apply(pending = control.state.pending) { return controller ? control.apply(pending) : control.confirm(pending); },
    async ready() { if (controller) control.open(); assert.equal(await this.refresh(), true); return this; },
    async draft(target = 'together', editing = false) {
      await this.ready();
      assert.equal(await (editing ? control.beginEdit(7) : control.beginCreate(action)), true);
      if (target !== null) assert.equal(control.edit('zone', target), true);
      return this;
    },
    async review(target = 'together', editing = false) {
      await this.draft(target, editing);
      assert.equal(await control.reviewCreate(), true, control.state.message);
      return control.state.pending;
    },
  };
}

function element(tag, attrs = {}, ...children) {
  return { tag, attrs, children, append(...next) { this.children.push(...next); },
    get textContent() { return [this.attrs.text || '', ...this.children.map(child => typeof child === 'string' ? child : child.textContent)].join(' '); } };
}

for (const surface of ['phone', 'dashboard']) {
  test(`${surface}: DJ routines offer aliases and household sets and review complete deduplicated groups`, async () => {
    const f = fixture(surface), pending = await f.review();
    const choices = f.control.state.options.rooms.map(item => item.target);
    for (const target of ['all', 'all@S1', 'all@S2', 'together', 'Member@S1', 'Lead@S2']) assert.ok(choices.includes(target), target);
    assert.equal(pending.body.zone, 'together');
    assert.equal(pending.destination.resolution.rooms.length, 3);
    assert.equal(pending.destination.resolution.groups.length, 2);
    assert.equal(f.writes().length, 0);
    const preview = S.routineTargets(pending, element).textContent;
    assert.match(preview, /Lead@S1/); assert.match(preview, /Lead@S2/);
    assert.match(preview, /Also affected through grouping: Guest@S1/);
    assert.match(preview, /not fixed future rooms/); assert.match(preview, /creator’s defaults and permissions/);
    assert.equal(f.calls.filter(call => call.url.startsWith('/rooms/resolve?')).length, 1, 'only the selected target is resolved at review');
    assert.ok(!f.calls.some(call => /\/(state|dj|queue)$/.test(call.url)), 'saving a future routine does not inspect or change playback');
  });

  test(`${surface}: all and household-qualified DJ routines save one dynamic definition after a second target read`, async () => {
    for (const [target, count] of [['all', 3], ['all@S1', 1], ['all@S2', 2]]) {
      const f = fixture(surface), pending = await f.review(target);
      assert.equal(pending.destination.resolution.groups.length, count);
      assert.equal(await f.apply(pending), true, f.control.state.message);
      assert.equal(await f.apply(pending), false);
      assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/schedules', body: pending.body }]);
      assert.equal(f.writes()[0].body.zone, target);
      assert.equal(f.writes()[0].body.expected_revision, undefined, 'future occurrences retain dynamic target semantics');
      assert.equal(f.calls.filter(call => call.url.startsWith('/rooms/resolve?')).length, 2);
      f.groups.push({ household: 'S2', coordinator_room: 'Later room', members: ['Later room'] });
      assert.equal(f.rows()[0].zone, target); assert.equal(f.writes().length, 1);
    }
  });

  test(`${surface}: edits preserve owner, paused state and exact schedule revision while changing to a room set`, async () => {
    const f = fixture(surface, [row({ zone: 'Lead@S1' })]), pending = await f.review('together', true);
    assert.equal(pending.request.expected_revision, 'saved-v1');
    assert.equal(pending.destination.resolution.groups.length, 2);
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.equal(f.rows()[0].creator, 'original creator'); assert.equal(f.rows()[0].enabled, false);
    assert.equal(f.rows()[0].zone, 'together'); assert.deepEqual(f.rows()[0].direction, row().direction);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/schedules/update');
  });

  test(`${surface}: alias expansion, physical drift and regrouping invalidate both create and edit reviews`, async () => {
    for (const editing of [false, true]) for (const change of [
      f => f.aliases.together.push('Kitchen@S2'),
      f => f.physical('a bonded player or its address changed'),
      f => f.groups[0].members.push('Another joined room'),
      f => { delete f.aliases.together; },
    ]) {
      const f = fixture(surface, editing ? [row()] : []), pending = await f.review('together', editing);
      change(f);
      assert.equal(await f.apply(pending), false);
      assert.equal(f.writes().length, 0);
      if (editing) { assert.equal(f.control.state.draft.zone, 'together'); assert.equal(f.control.state.result.status, 'not_sent'); }
      assert.equal(await f.apply(pending), false);
    }
  });

  test(`${surface}: closing or cancelling a delayed target read cannot publish a usable stale review`, async () => {
    for (const end of ['close', 'cancel']) {
      const f = await fixture(surface).draft(), gate = defer();
      f.intercept(url => url.startsWith('/rooms/resolve?') ? gate.promise : undefined);
      const pending = f.control.reviewCreate();
      assert.equal(f.control.state.loading, true);
      f.control[end](); gate.resolve(f.resolve('together'));
      assert.equal(await pending, false); assert.equal(f.control.state.pending, null);
      assert.equal(f.control.state.loading, false); assert.equal(f.writes().length, 0);
      assert.equal(await f.apply(), false);
    }
  });

  test(`${surface}: missing physical revisions and contradictory targets cannot become a schedule review`, async () => {
    for (const invalid of [value => { delete value.revision; }, value => { value.groups.push(copy(value.groups[0])); }]) {
      const f = await fixture(surface).draft();
      f.intercept(url => {
        if (!url.startsWith('/rooms/resolve?')) return;
        const value = f.resolve('together'); invalid(value); return value;
      });
      assert.equal(await f.control.reviewCreate(), false);
      assert.equal(f.control.state.pending, null); assert.equal(f.writes().length, 0);
      assert.equal(f.control.state.draft.zone, 'together');
    }
  });

  test(`${surface}: an offline saved target stays explicit and daemon validation failure retains its draft without replay`, async () => {
    const f = fixture(surface, [row({ zone: 'Offline room@S1' })]), pending = await f.review(null, true);
    assert.equal(pending.body.zone, 'Offline room@S1');
    assert.ok(pending.destination.unresolved);
    assert.match(S.routineTargets(pending, element).textContent, /not currently resolved/);
    f.reject(Object.assign(new Error('UNKNOWN_ROOM: saved target is absent'), { code: 'UNKNOWN_ROOM' }));
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 1);
    assert.equal(f.control.state.draft.zone, 'Offline room@S1');
    assert.ok(f.control.state.recovery, 'an unverified edit requires observing the stored row');
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 1);
    await f.refresh(); assert.equal(f.control.state.recovery, null);
    assert.equal(f.control.state.draft.zone, 'Offline room@S1');
  });

  test(`${surface}: an offline retained target returning before confirmation requires a newly visible audience review`, async () => {
    const f = fixture(surface, [row({ zone: 'Offline room@S1' })]), pending = await f.review(null, true);
    f.groups.push({ household: 'S1', coordinator_room: 'Offline room', members: ['Offline room'] });
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 0);
    assert.equal(f.control.state.draft.zone, 'Offline room@S1');
    await f.refresh(); assert.equal(await f.control.reloadDraft(), true);
    assert.equal(await f.control.reviewCreate(), true);
    assert.equal(f.control.state.pending.destination.resolution.groups.length, 1);
  });

  test(`${surface}: a saved target in a missing household stays explicit while new targets still require resolution`, async () => {
    const f = await fixture(surface, [row({ zone: 'Bedroom@MissingHousehold' })]).draft(null, true);
    f.intercept(url => {
      if (url.startsWith('/rooms/resolve?')) throw Object.assign(new Error('The saved household is not currently discovered'), { code: 'UNKNOWN_HOUSEHOLD' });
    });
    assert.equal(await f.control.reviewCreate(), true, f.control.state.message);
    assert.equal(f.control.state.pending.body.zone, 'Bedroom@MissingHousehold');
    assert.match(S.routineTargets(f.control.state.pending, element).textContent, /not currently discovered/);
    assert.equal(f.writes().length, 0);
    const next = await fixture(surface).draft();
    next.intercept(url => {
      if (url.startsWith('/rooms/resolve?')) throw Object.assign(new Error('The target disappeared'), { code: 'UNKNOWN_HOUSEHOLD' });
    });
    assert.equal(await next.control.reviewCreate(), false);
    assert.equal(next.control.state.pending, null); assert.equal(next.writes().length, 0);
  });

  test(`${surface}: a saved alias advertised by an online member can retain an explicitly unresolved review`, async () => {
    const f = await fixture(surface, [row()]).draft('together', true);
    assert.ok(f.control.state.options.rooms.some(room => room.target === 'together'));
    f.intercept(url => {
      if (url.startsWith('/rooms/resolve?')) throw Object.assign(new Error('One saved alias member is still offline'), { code: 'NOT_READY' });
    });
    assert.equal(await f.control.reviewCreate(), true, f.control.state.message);
    assert.equal(f.control.state.pending.body.zone, 'together');
    assert.match(S.routineTargets(f.control.state.pending, element).textContent, /saved alias member is still offline/);
    assert.equal(f.writes().length, 0);
    const fresh = await fixture(surface).draft('together');
    fresh.intercept(url => {
      if (url.startsWith('/rooms/resolve?')) throw Object.assign(new Error('One alias member is still offline'), { code: 'NOT_READY' });
    });
    assert.equal(await fresh.control.reviewCreate(), false); assert.equal(fresh.writes().length, 0);
  });
}

for (const surface of ['phone', 'dashboard']) {
  test(surface + ': editing here distinguishes the current browser preview from the saved creator default', async () => {
    const f = fixture(surface, [row({ zone: 'here' })]);
    f.aliases.here = ['Lead@S2'];
    f.intercept(url => {
      if (url !== '/rooms/resolve?target=here') return;
      return { ...f.resolve('here'), matched: 'here' };
    });
    const pending = await f.review(null, true);
    assert.equal(pending.destination.resolution.groups.length, 1);
    assert.equal(pending.destination.resolution.groups[0].household, 'S2');
    const node = S.routineTargets(pending, element), preview = node.textContent;
    assert.equal(node.attrs['aria-label'], 'Current browser default preview');
    assert.match(preview, /Current browser default preview/);
    assert.match(preview, /original creator/);
    assert.match(preview, /may already.*different rooms/i);
    assert.doesNotMatch(preview, /Each group starts once|Affected rooms:/);
    assert.match(preview, /Lead@S2/);
    assert.equal(pending.body.zone, 'here');
    assert.equal(pending.request.expected_revision, 'saved-v1');
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.equal(f.writes().length, 1);
    assert.equal(f.writes()[0].body.schedule.zone, 'here');
    assert.equal(f.rows()[0].creator, 'original creator');
    assert.equal(f.rows()[0].enabled, false);
  });
}

const pauseRow = extra => row({ action: 'pause', mood: null, direction: null, fade_secs: 120,
  summary: 'Saved faded bedtime pause', ...extra });

for (const surface of ['phone', 'dashboard']) {
  test(`${surface}: faded bedtime routines review every aliased group without reading current music or DJ settings`, async () => {
    const f = await fixture(surface, [], 'pause').draft();
    f.control.edit('fade_secs', '120');
    assert.equal(await f.control.reviewCreate(), true, f.control.state.message);
    const pending = f.control.state.pending;
    assert.equal(pending.destination.resolution.groups.length, 2);
    assert.equal(pending.body.zone, 'together'); assert.equal(pending.body.fade_secs, 120);
    const view = S.routineTargets(pending, element);
    assert.equal(view.attrs['aria-label'], 'Currently affected pause groups');
    assert.match(view.textContent, /Lead@S1/); assert.match(view.textContent, /Lead@S2/);
    assert.match(view.textContent, /Also affected through grouping: Guest@S1/);
    assert.match(view.textContent, /fades out over 120s, pauses, and has its original volumes restored/);
    assert.match(view.textContent, /No music pauses or fades while saving/);
    assert.doesNotMatch(view.textContent, /Each group starts once|Current DJ target/);
    assert.ok(!f.calls.some(call => /\/(state|dj|queue|moods)$/.test(call.url)));
    assert.equal(f.writes().length, 0);
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/schedules', body: pending.body }]);
    assert.equal(f.calls.filter(call => call.url.startsWith('/rooms/resolve?')).length, 2);
    assert.equal(pending.body.expected_revision, undefined, 'future occurrences keep the authored dynamic target');
  });

  test(`${surface}: immediate and faded pause schedules accept all, household and member-room targets`, async () => {
    for (const [target, count] of [['all', 3], ['all@S1', 1], ['all@S2', 2], ['Member@S1', 1]]) {
      for (const seconds of ['0', '600']) {
        const f = await fixture(surface, [], 'pause').draft(target);
        f.control.edit('fade_secs', seconds);
        assert.equal(await f.control.reviewCreate(), true, f.control.state.message);
        const pending = f.control.state.pending;
        assert.equal(pending.destination.resolution.groups.length, count);
        assert.match(S.routineTargets(pending, element).textContent,
          seconds === '0' ? /Each group pauses once/ : /fades out over 600s/);
        assert.equal(await f.apply(pending), true, f.control.state.message);
        assert.equal(f.rows()[0].zone, target); assert.equal(f.rows()[0].fade_secs, Number(seconds));
        assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 1);
      }
    }
  });

  test(`${surface}: a faded pause edit preserves the creator and disabled state while replacing its room target`, async () => {
    const f = fixture(surface, [pauseRow({ zone: 'Lead@S1' })], 'pause');
    const pending = await f.review('together', true);
    assert.equal(pending.request.expected_revision, 'saved-v1');
    assert.equal(pending.request.schedule.fade_secs, 120);
    assert.equal(pending.destination.resolution.groups.length, 2);
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.equal(f.rows()[0].creator, 'original creator'); assert.equal(f.rows()[0].enabled, false);
    assert.equal(f.rows()[0].zone, 'together'); assert.equal(f.rows()[0].fade_secs, 120);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/schedules/update');
  });

  test(`${surface}: a changed bedtime audience invalidates both a new routine and a reviewed edit before persistence`, async () => {
    for (const editing of [false, true]) for (const change of [
      f => f.aliases.together.push('Kitchen@S2'),
      f => f.physical('a bonded player or its address changed'),
      f => f.groups[0].members.push('Later joined room'),
      f => { delete f.aliases.together; },
    ]) {
      const f = fixture(surface, editing ? [pauseRow()] : [], 'pause');
      const pending = await f.review('together', editing); change(f);
      assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 0);
      if (editing) { assert.equal(f.control.state.draft.zone, 'together'); assert.equal(f.control.state.result.status, 'not_sent'); }
    }
  });

  test(`${surface}: missing bedtime revisions and cancelled in-flight reviews cannot authorize a saved routine`, async () => {
    const malformed = await fixture(surface, [], 'pause').draft();
    malformed.intercept(url => {
      if (!url.startsWith('/rooms/resolve?')) return;
      const value = malformed.resolve('together'); delete value.revision; return value;
    });
    assert.equal(await malformed.control.reviewCreate(), false); assert.equal(malformed.writes().length, 0);
    for (const method of ['cancel', 'close']) {
      const f = await fixture(surface, [], 'pause').draft(), gate = defer();
      f.intercept(url => url.startsWith('/rooms/resolve?') ? gate.promise : undefined);
      const reading = f.control.reviewCreate(); f.control[method](); gate.resolve(f.resolve('together'));
      assert.equal(await reading, false); assert.equal(f.control.state.pending, null);
      assert.equal(await f.apply(), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${surface}: an offline saved bedtime target stays explicit and a lost accepted edit is never replayed`, async () => {
    const offline = fixture(surface, [pauseRow({ zone: 'Offline room@S1' })], 'pause');
    const retained = await offline.review(null, true);
    assert.equal(retained.body.zone, 'Offline room@S1'); assert.ok(retained.destination.unresolved);
    assert.match(S.routineTargets(retained, element).textContent, /not currently resolved/);
    offline.groups.push({ household: 'S1', coordinator_room: 'Offline room', members: ['Offline room'] });
    assert.equal(await offline.apply(retained), false); assert.equal(offline.writes().length, 0);
    const f = await fixture(surface, [pauseRow()], 'pause').draft('all', true);
    f.control.edit('fade_secs', '60'); assert.equal(await f.control.reviewCreate(), true);
    const pending = f.control.state.pending; f.lose();
    assert.equal(await f.apply(pending), false); assert.equal(f.control.state.result.status, 'uncertain');
    assert.equal(f.rows()[0].zone, 'all'); assert.equal(f.rows()[0].fade_secs, 60);
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 1);
    await f.refresh(); assert.equal(f.control.state.recovery, null);
    assert.equal(f.writes().length, 1);
  });

  test(`${surface}: editing a bedtime here target describes the original creator's future default`, async () => {
    const f = fixture(surface, [pauseRow({ zone: 'here' })], 'pause');
    f.aliases.here = ['Lead@S2'];
    const pending = await f.review(null, true), view = S.routineTargets(pending, element);
    assert.equal(view.attrs['aria-label'], 'Current browser default preview');
    assert.match(view.textContent, /original creator/); assert.match(view.textContent, /may already.*different rooms/);
    assert.doesNotMatch(view.textContent, /Each group pauses once|Each active group fades/);
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.equal(f.rows()[0].zone, 'here'); assert.equal(f.rows()[0].creator, 'original creator');
  });
}
