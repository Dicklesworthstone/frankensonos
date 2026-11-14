'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const S = require('../crates/fsonos-api/src/remote/schedules.js');
const R = require('../crates/fsonos-api/src/remote/dashboard-routines.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const targets = require('./schedule-targets-fixture.cjs');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(yes => { resolve = yes; }); return { promise, resolve }; };
async function settled(predicate) {
  for (let n = 0; n < 300 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'expected asynchronous boundary was reached');
}
const definitions = {
  dj_start: { zone: 'Study@SIM_S2', mood: 'calm', direction: { constraints: { include_artists: ['Synthetic artist'] }, for_secs: 17 } },
  pause: { zone: 'all', fade_secs: null },
  volume: { zone: 'upstairs', volume: 26 },
  apply_scene: { scene: 'Morning fixture' },
  announce: { announcement: { text: 'Synthetic reminder', voice: 'fixture_voice', rooms: [], volume: null } },
};
function row(action = 'pause', change = {}) {
  return { id: 21, revision: 'opaque-version-A', when: 'weekdays 07:30', action,
    zone: null, mood: null, scene: null, volume: null, fade_secs: null,
    ...copy(definitions[action]), summary: `Synthetic ${action} routine`, creator: 'Fixture creator',
    enabled: true, next: '2030-06-17T07:30:00-04:00', last_fired: '2030-06-14T07:30:00-04:00', ...change };
}
function documentFixture() {
  const roots = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', type: '', disabled: false, open: false,
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      set innerHTML(_) { throw new Error('Untrusted routine metadata must remain text'); },
      setAttribute(name, value) { this.attributes[name] = String(value); if (name === 'id' || name === 'type') this[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this._text = ''; this.children = children; },
      contains(node) { return node === this || descendants(this).includes(node); },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(node => node.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; }, setSelectionRange(start) { this.selectionStart = start; } };
  }, getElementById(id) { return roots.get(id) || [...roots.values()].flatMap(descendants).find(node => node.id === id) || null; } };
  for (const id of ['routines-dialog', 'routines-body', 'open-routines', 'close-routines', 'routines-read',
    'routines-create-scene', 'routines-create-dj', 'routines-message']) roots.set(id, doc.createElement('div'));
  return doc;
}
function fixture(surface, { rows = [row()], dom = false, network } = {}) {
  const zones = [{ household: 'SIM_S2', coordinator_room: 'Study', members: ['Study', 'Hall'], transport_state: 'paused' }];
  const scenes = [{ name: 'Morning fixture', saved: 17, groups: [], volumes: {}, mutes: {} }];
  const moods = { moods: [{ name: 'calm', constraints: {} }] }, calls = [];
  let schedules = copy(rows), reader = null, writer = null, time = 1000, control;
  const read = url => {
    if (url === '/schedules') return copy(schedules);
    if (url === '/zones') return copy(zones);
    if (url === '/rooms') return targets.rooms(zones);
    if (url.startsWith('/rooms/resolve?')) return targets.resolve(zones, new URL(url, 'http://fixture').searchParams.get('target'));
    if (url === '/scenes') return copy(scenes);
    if (url === '/dj/moods') return copy(moods);
    if (url.endsWith('/state')) return { zone: copy(zones[0]), transport_state: 'paused', volume: 20,
      track: { uri: 'fixture:music', queue_position: 2 } };
    throw new Error('Unexpected fixture read: ' + url);
  };
  const write = (url, request) => {
    assert.equal(url, '/schedules/update');
    const index = schedules.findIndex(item => item.id === request.id), old = schedules[index];
    assert.ok(old, 'fixture requires the original ID');
    assert.equal(request.expected_revision, old.revision);
    const body = request.schedule;
    const when = body.when.startsWith('in ') ? 'once 2030-06-20T19:00:00+09:00'
      : /^\d{4}-/.test(body.when) ? 'once ' + body.when : body.when;
    const actionSignature = value => JSON.stringify([value.action, value.zone ?? null, value.mood ?? null, value.scene ?? null,
      value.volume ?? null, value.action === 'pause' ? value.fade_secs ?? 0 : null, value.direction ?? null,
      value.announcement == null ? null : S.announcementOf(value.announcement)]);
    const changed = when !== old.when || actionSignature(body) !== actionSignature(old);
    const result = { id: old.id, creator: old.creator, enabled: old.enabled, last_fired: old.last_fired,
      revision: changed ? old.revision + '.next' : old.revision, when, action: body.action,
      zone: body.zone ?? null, mood: body.mood ?? null, scene: body.scene ?? null, volume: body.volume ?? null, fade_secs: body.fade_secs || null,
      ...(body.direction !== undefined ? { direction: copy(body.direction) } : {}),
      ...(body.announcement !== undefined ? { announcement: copy(body.announcement) } : {}),
      summary: changed ? `Updated synthetic ${body.action} routine` : old.summary,
      next: old.enabled ? when.startsWith('once ') ? when.slice(5) : '2030-06-18T08:45:00-04:00' : null };
    schedules[index] = copy(result); return result;
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, options, read) : read(url)
      : writer ? writer(url, body, write) : write(url, body);
  };
  const call = network ? network(raw) : raw;
  const phoneCall = (name, options = {}) => call(S.routes[name][1]
    + (options.query ? '?' + new URLSearchParams(options.query) : ''), options.body, options);
  const controller = surface === 'dashboard' ? C.model({ call, now: () => time, changed: () => control?.observe() }) : null;
  const document = dom ? documentFixture() : null;
  if (dom) control = R.panel({ call, controller, document, now: () => time }).control;
  else control = surface === 'dashboard' ? R.model({ call, controller, now: () => time }) : S.model({ call: phoneCall });
  return { surface, control, controller, document, call, phoneCall, calls, zones, scenes, moods,
    reads: () => calls.filter(call => call.body === undefined), writes: () => calls.filter(call => call.body !== undefined),
    rows: () => copy(schedules), setRows(value) { schedules = copy(value); },
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    refresh(explicit = true) { return surface === 'dashboard' ? control.read(explicit) : control.refresh(explicit); },
    apply(reviewed = control.state.pending) { return surface === 'dashboard' ? control.apply(reviewed) : control.confirm(reviewed); },
    async ready() { if (surface === 'dashboard') assert.equal(control.open(), true); assert.equal(await this.refresh(false), true); return this; } };
}
async function edit(f, id = 21, time = '08:45') {
  assert.equal(await f.control.beginEdit(id), true);
  if (time !== null) assert.equal(f.control.edit('time', time), true);
  assert.equal(await f.control.reviewCreate(), true, f.control.state.message);
  return f.control.state.pending;
}

for (const surface of ['phone', 'dashboard']) {
  for (const action of Object.keys(definitions)) test(`${surface}: ${action} edits retain ID, creator and paused state with one native request`, async () => {
    const original = row(action, { enabled: false, next: null }), f = await fixture(surface, { rows: [original] }).ready();
    const pending = await edit(f);
    assert.equal(pending.action, 'update'); assert.equal(pending.row.id, 21);
    assert.ok(Object.isFrozen(pending.request)); assert.ok(Object.isFrozen(pending.request.schedule));
    assert.deepEqual(pending.request, { id: 21, expected_revision: 'opaque-version-A', schedule: pending.body });
    assert.equal(await f.apply(pending), true, f.control.state.message);
    assert.equal(await f.apply(pending), false);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/schedules/update', body: pending.request }]);
    assert.equal(f.control.state.result.row.id, 21); assert.equal(f.control.state.result.row.creator, original.creator);
    assert.equal(f.control.state.result.row.enabled, false); assert.equal(f.control.state.result.row.next, null);
    assert.equal(f.control.state.result.status, 'accepted');
    assert.ok(f.calls.every(({ url }) => ['/schedules', '/schedules/update', '/zones', '/rooms', '/scenes', '/dj/moods'].includes(url)
      || url.startsWith('/rooms/resolve?')));
    assert.match(S.updateEffect(pending.row), /future occurrences/); assert.match(S.updateEffect(pending.row), /cannot be undone/);
    if (action === 'announce') assert.deepEqual(pending.body.announcement, { text: 'Synthetic reminder', voice: 'fixture_voice', rooms: [] });
    else if (action !== 'apply_scene') assert.equal(pending.body.zone, original.zone);
    if (action === 'dj_start') assert.deepEqual(pending.body.direction, original.direction);
  });

  test(`${surface}: verified same-definition edits of all five actions exclude their own ID from duplicate checks`, async () => {
    for (const action of Object.keys(definitions)) {
      const f = await fixture(surface, { rows: [row(action)] }).ready(), pending = await edit(f, 21, null);
      assert.equal(await f.apply(pending), true, f.control.state.message); assert.equal(f.control.state.result.row.revision, 'opaque-version-A');
      assert.equal(f.writes().length, 1);
    }
  });

  test(`${surface}: changing action keeps the original routine identity and discards incompatible fields`, async () => {
    const f = await fixture(surface, { rows: [row('pause')] }).ready();
    await edit(f);
    const old = f.control.state.pending;
    assert.equal(await f.control.changeAction('announce'), true);
    assert.equal(await f.apply(old), false);
    assert.equal(f.control.state.draft.time, '08:45');
    f.control.edit('announcement_source', 'chime'); f.control.edit('announcement_chime', 'rise');
    f.control.edit('announcement_rooms', ['Hall@SIM_S2']); f.control.edit('announcement_volume', '0');
    assert.equal(await f.control.reviewCreate(), true); const reviewed = f.control.state.pending;
    assert.deepEqual(reviewed.body, { when: 'weekdays 08:45', action: 'announce', announcement: { chime: 'rise', rooms: ['Hall@SIM_S2'], volume: 0 } });
    assert.equal(await f.apply(reviewed), true); assert.equal(f.writes().length, 1); assert.equal(f.rows()[0].id, 21);
  });

  test(`${surface}: editing the new time consumes the older review without silently changing its target`, async () => {
    const f = await fixture(surface).ready(), prior = await edit(f);
    f.control.edit('time', '09:30'); assert.equal(await f.apply(prior), false);
    assert.equal(await f.control.reviewCreate(), true); assert.equal(f.control.state.pending.request.id, 21);
    assert.equal(prior.body.when, 'weekdays 08:45'); assert.equal(f.control.state.pending.body.when, 'weekdays 09:30');
    assert.equal(await f.apply(), true); assert.equal(f.writes().length, 1);
  });

  test(`${surface}: missing revisions preserve old-daemon reads and refuse edits`, async () => {
    const f = await fixture(surface, { rows: [row('pause', { revision: undefined })] }).ready();
    assert.equal(f.control.state.rows.length, 1); assert.equal(await f.control.beginEdit(21), false);
    assert.match(f.control.state.message, /revision|updated daemon/); assert.equal(f.writes().length, 0);
    assert.equal(f.control.review('pause', 21), true);
  });

  for (const [name, mutate] of [
    ['scheduler claim', value => ({ ...value, revision: 'claimed-A', last_fired: '2030-06-17T07:30:00-04:00' })],
    ['definition', value => ({ ...value, when: 'daily 10:00' })],
    ['creator', value => ({ ...value, creator: 'Another fixture' })],
    ['enabled state', value => ({ ...value, enabled: false, next: null })],
    ['deletion', () => null],
  ]) test(`${surface}: ${name} changed before submission rejects the anchored edit without sending`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f), changed = mutate(row());
    f.setRows(changed ? [changed] : []);
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 0);
    assert.equal(f.control.state.result.status, 'not_sent'); assert.equal(f.control.state.draft.time, '08:45');
    assert.match(f.control.state.result.detail, /changed, ran or was removed/);
    await f.refresh(); assert.equal(await f.control.reloadDraft(), true);
    assert.equal(await f.control.reviewCreate(), false, 'a refreshed list must not silently rebase the saved revision');
  });

  test(`${surface}: changed current room topology invalidates an edit and saved aliases cannot be invented`, async () => {
    const f = await fixture(surface, { rows: [row('volume', { zone: 'Hall@SIM_S2' })] }).ready();
    await edit(f); f.zones[0].members.push('Synthetic new room');
    assert.equal(await f.apply(), false); assert.equal(f.writes().length, 0);
    const alias = await fixture(surface, { rows: [row('volume')] }).ready();
    await edit(alias); assert.ok(alias.control.state.options.rooms.some(room => room.target === 'upstairs'));
    alias.control.edit('zone', 'invented-alias'); assert.equal(await alias.control.reviewCreate(), false);
    assert.equal(alias.writes().length, 0);
  });

  test(`${surface}: a newly duplicated routine is checked again before dispatch`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f);
    f.setRows([row(), row('pause', { id: 22, when: pending.body.when, revision: 'other-record' })]);
    assert.equal(await f.apply(pending), false); assert.equal(f.writes().length, 0);
    assert.match(f.control.state.result.detail, /equivalent schedule #22/);
  });

  test(`${surface}: closing during preflight preserves the draft and sends no request`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f), gate = deferred();
    f.read(async (url, options, normal) => { if (url === '/schedules') await gate.promise; return normal(url); });
    const applying = f.apply(pending); f.control.close(); gate.resolve();
    assert.equal(await applying, false); assert.equal(f.writes().length, 0);
    assert.equal(f.control.state.pending, null); assert.equal(f.control.state.draft.time, '08:45');
  });

  test(`${surface}: accepted edits stay single-flight and keep their result when the panel closes`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f), gate = deferred();
    f.write(async (url, request, normal) => { await gate.promise; return normal(url, request); });
    const applying = f.apply(pending); await settled(() => f.writes().length === 1);
    f.control.close(); assert.equal(await f.apply(pending), false); assert.equal(await f.refresh(), false);
    if (f.controller) assert.equal(await f.controller.mutate(() => { throw new Error('Must stay blocked'); }, { stored: 'schedules' }), false);
    gate.resolve(); assert.equal(await applying, true);
    assert.equal(f.control.state.result.status, 'accepted'); assert.equal(f.control.state.result.row.id, 21);
    assert.equal(f.control.state.draft, null); assert.equal(f.writes().length, 1);
    if (f.controller) f.control.open(); await f.refresh(); assert.equal(f.control.state.result.status, 'accepted');
  });

  test(`${surface}: a lost reply retains the draft and needs an explicit stored-list observation`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f);
    f.write((url, request, normal) => { normal(url, request); throw new Error('Synthetic reply lost'); });
    assert.equal(await f.apply(pending), false); assert.equal(f.control.state.result.status, 'uncertain');
    assert.equal(f.control.state.draft.time, '08:45'); assert.equal(await f.apply(pending), false);
    if (f.controller) { assert.equal(f.controller.state.uncertain, true); await f.controller.refresh(true); assert.equal(f.controller.state.uncertain, true); }
    await f.refresh(false); assert.equal(f.control.state.result.status, 'uncertain'); assert.equal(await f.control.beginEdit(21), false);
    await f.refresh(true); assert.equal(f.control.state.result.status, 'observed'); assert.equal(f.control.state.recovery, null);
    assert.match(f.control.state.result.detail, /does not prove/); assert.match(f.control.state.result.detail, /#21/);
    assert.equal(f.control.state.draft.time, '08:45'); assert.equal(await f.control.reloadDraft(), true);
    assert.equal(await f.control.reviewCreate(), false, 'old expected revision cannot target the new record');
    assert.equal(f.writes().length, 1);
  });

  test(`${surface}: a conflict after preflight is never retried or rebased`, async () => {
    const f = await fixture(surface).ready(), pending = await edit(f);
    f.write(() => { const error = new Error('ConfigChanged: routine ran after review'); error.status = 409; throw error; });
    assert.equal(await f.apply(pending), false); assert.equal(await f.apply(pending), false);
    assert.equal(f.control.state.draft.time, '08:45'); assert.equal(f.writes().length, 1);
    assert.match(f.control.state.result.detail, /ConfigChanged/);
    await f.refresh(true); assert.equal(f.writes().length, 1); assert.equal(f.control.state.pending, null);
  });

  test(`${surface}: unreadable recovery data cannot release an uncertain edit`, async () => {
    const f = await fixture(surface).ready(); await edit(f);
    f.write(() => { throw new Error('Lost acknowledgement'); }); await f.apply();
    f.read((url, options, normal) => url === '/schedules' ? [row('pause', { revision: '' })] : normal(url));
    assert.equal(await f.refresh(true), false); assert.equal(f.control.state.result.status, 'uncertain');
    assert.equal(await f.control.beginEdit(21), false); assert.equal(f.writes().length, 1);
  });
}

test('hydration round-trips exact once offsets, custom days, legacy DJ steering, and announcement defaults', async () => {
  const f = fixture('phone');
  for (const original of [
    row('pause', { when: 'once 2030-07-01T18:30:00-05:00', next: '2030-07-01T23:30:00+00:00', zone: 'everywhere' }),
    row('volume', { when: 'mon,wed,fri 21:05', zone: 'downstairs' }),
    row('dj_start', { direction: undefined }),
    row('dj_start', { direction: { constraints: {} } }),
    row('announce', { announcement: { chime: 'bell', rooms: ['upstairs'], volume: 0 } }),
    row('announce'),
  ]) {
    const draft = S.draftFromSchedule(original), options = await S.choices(f.phoneCall, draft.action, S.scheduleOf(original));
    const reviewed = S.updatePlan(S.scheduleOf(original), draft, options, [S.scheduleOf(original)]);
    assert.equal(reviewed.body.action, original.action);
    if (original.action === 'dj_start') assert.deepEqual(reviewed.body.direction, original.direction);
    if (original.when.startsWith('once ')) assert.equal(reviewed.body.when, original.when.slice(5));
    else assert.equal(reviewed.body.when, original.when);
    assert.equal(S.updatedOf(original, reviewed).id, original.id);
  }
});

test('editing a DJ filter keeps a saved non-minute duration until the owner edits that duration', async () => {
  for (const surface of ['phone', 'dashboard']) {
    const f = await fixture(surface, { rows: [row('dj_start')] }).ready(); await edit(f);
    f.control.editDirection('include_genres', 'Synthetic genre'); assert.equal(await f.control.reviewCreate(), true);
    assert.equal(f.control.state.pending.body.direction.for_secs, 17);
    f.control.editDirection('minutes', '2'); assert.equal(await f.control.reviewCreate(), true);
    assert.equal(f.control.state.pending.body.direction.for_secs, 120);
    f.control.editDirection('minutes', ''); assert.equal(await f.control.reviewCreate(), true);
    assert.equal(f.control.state.pending.body.direction.for_secs, undefined);
  }
});

test('edit acknowledgements must confirm the original identity, state, exact action and new revision', async () => {
  const f = await fixture('phone').ready(), pending = await edit(f);
  const good = { ...row(), when: pending.body.when, revision: 'opaque-version-B' };
  assert.equal(S.updatedOf(good, pending).id, 21);
  for (const mutation of [
    { id: 22 }, { creator: 'Changed creator' }, { enabled: false, next: null }, { when: 'daily 21:00' },
    { zone: 'elsewhere' }, { fade_secs: 9 }, { revision: null }, { revision: '' }, { revision: 'opaque-version-A' }, { next: null },
  ]) assert.throws(() => S.updatedOf({ ...good, ...mutation }, pending), /reply|unreadable/);
  for (const revision of ['', 'contains\nnewline', {}, 3]) assert.throws(() => S.scheduleOf(row('pause', { revision })), /unreadable/);
});

test('canonical paused receipts omit zero fade and next, preserve state across action changes, and may advance the cutoff', async () => {
  for (const surface of ['phone', 'dashboard']) {
    const original = row('pause', { fade_secs: undefined, enabled: false, next: undefined });
    const f = await fixture(surface, { rows: [original] }).ready(), unchanged = await edit(f, 21, null);
    assert.equal(unchanged.body.fade_secs, 0);
    assert.equal(S.updatedOf(original, unchanged).revision, original.revision);
    assert.equal(await f.control.changeAction('volume'), true); f.control.edit('zone', 'Hall@SIM_S2'); f.control.edit('volume', '0');
    assert.equal(await f.control.reviewCreate(), true); const reviewed = f.control.state.pending;
    f.write((url, request) => {
      assert.deepEqual(request, reviewed.request);
      return { id: 21, revision: 'opaque-version-B', when: 'weekdays 07:30', action: 'volume', zone: 'Hall@SIM_S2', volume: 0,
        creator: 'Fixture creator', enabled: false, summary: 'Set Hall to zero', last_fired: '2030-06-16T18:00:00+00:00' };
    });
    assert.equal(await f.apply(reviewed), true); assert.equal(f.control.state.result.row.next, null);
    assert.equal(f.control.state.result.row.fade_secs, null); assert.equal(f.control.state.result.row.volume, 0);
    assert.equal(f.control.state.result.row.last_fired, '2030-06-16T18:00:00+00:00');
  }
});

test('one-shot receipts accept canonical offsets and completed no-ops but reject contradictory future deadlines', async () => {
  const original = row('pause', { when: 'once 2030-07-01T18:30:00-05:00', next: null });
  const f = await fixture('phone', { rows: [original] }).ready();
  const unchanged = await edit(f, 21, null);
  assert.equal(S.updatedOf(original, unchanged).next, null);
  f.control.edit('at', '2030-07-02T23:30:00Z'); assert.equal(await f.control.reviewCreate(), true);
  const reviewed = f.control.state.pending;
  const good = { ...original, revision: 'opaque-version-B', when: 'once 2030-07-02T23:30:00+00:00', next: '2030-07-02T19:30:00-04:00' };
  assert.equal(S.updatedOf(good, reviewed).id, 21);
  assert.throws(() => S.updatedOf({ ...good, next: '2030-07-02T23:31:00Z' }, reviewed), /reply/);
  assert.throws(() => S.updatedOf({ ...good, next: null }, reviewed), /reply/);
});

test('dashboard DOM opens every saved action in the shared form and shows retained targets and non-undoable review', async () => {
  for (const action of Object.keys(definitions)) {
    const f = await fixture('dashboard', { rows: [row(action)], dom: true }).ready();
    const controls = () => f.document.getElementById('routines-body').querySelectorAll('[data-routine-focus]');
    const click = key => { const target = controls().find(node => node.getAttribute('data-routine-focus') === key); assert.ok(target, key); target.fire('click'); };
    click('edit-21'); await settled(() => f.control.state.options);
    assert.equal(f.document.getElementById('routine-action').value, action);
    assert.equal(f.document.getElementById('routine-action').children.length, 5);
    if (['pause', 'volume'].includes(action)) assert.match(f.document.getElementById('routine-zone').textContent, /Saved target/);
    if (action === 'announce') assert.equal(f.document.getElementById('routine-announcement_text').value, 'Synthetic reminder');
    const input = f.document.getElementById('routine-time'); input.value = '08:45'; input.fire('input');
    click('review-create');
    await settled(() => f.control.state.pending && !f.control.state.loading);
    const review = f.document.getElementById('routines-body').textContent;
    assert.match(review, /routine #21/); assert.match(review, /Previously: weekdays 07:30/);
    assert.match(review, /weekdays 08:45/); assert.match(review, /cannot be undone/); assert.match(review, /No music starts now/);
    click('confirm'); await settled(() => f.control.state.result?.status === 'accepted');
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/schedules/update');
  }
});

test('phone DOM edits the exact stored routine and retains draft text after an uncertain acknowledgement', async () => {
  const f = fixture('phone', { rows: [row('announce')] }), doc = documentFixture();
  const prior = global.document; global.document = doc;
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
      else if (['value', 'checked', 'disabled', 'open'].includes(key)) node[key] = value;
      else node.setAttribute(key, value);
    }
    node.append(...children); return node;
  };
  try {
    const panel = S.panel({ call: f.phoneCall, el }), node = panel.node;
    const find = key => node.querySelectorAll('[data-schedule-focus]').find(child => child.getAttribute('data-schedule-focus') === key);
    node.open = true; node.fire('toggle'); await settled(() => find('edit-21') && !find('edit-21').disabled);
    find('edit-21').fire('click'); await settled(() => find('announcement_text') && !find('announcement_text').disabled);
    const text = find('announcement_text'); text.value = 'Owner edited fixture reminder'; text.fire('input');
    find('review-create').fire('click'); assert.match(node.textContent, /routine #21/); assert.match(node.textContent, /cannot be undone/);
    f.write(() => { throw new Error('Synthetic lost response'); }); find('confirm').fire('click');
    await settled(() => node.textContent.includes('edit may have been saved'));
    assert.equal(find('announcement_text').value, 'Owner edited fixture reminder');
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].body.id, 21);
    panel.dispose();
  } finally { global.document = prior; }
});

test('real loopback HTTP uses POST /schedules/update with the exact nested revision contract for all five actions', async () => {
  let handler;
  const server = http.createServer(async (req, res) => {
    try {
      let raw = ''; for await (const part of req) raw += part;
      const body = raw ? JSON.parse(raw) : undefined;
      assert.equal(req.method, body === undefined ? 'GET' : 'POST');
      const result = await handler(req.url, body);
      res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(result));
    } catch (error) { res.statusCode = 500; res.end(JSON.stringify({ message: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const base = `http://127.0.0.1:${server.address().port}`;
    for (const action of Object.keys(definitions)) {
      const f = await fixture('dashboard', { rows: [row(action)], network: raw => {
        handler = raw;
        return async (url, body, options) => {
          const response = await fetch(base + url, { method: body === undefined ? 'GET' : 'POST',
            headers: { 'content-type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: options?.signal });
          const value = await response.json(); if (!response.ok) throw new Error(value.message); return value;
        };
      } }).ready();
      const reviewed = await edit(f); assert.equal(await f.apply(reviewed), true, f.control.state.message);
      assert.equal(f.writes().length, 1); assert.deepEqual(f.writes()[0].body, reviewed.request);
      assert.equal(f.controller.state.uncertain, false);
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
});
