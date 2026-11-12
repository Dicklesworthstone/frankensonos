'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const R = require('../crates/fsonos-api/src/remote/dashboard-routines.js');
const S = require('../crates/fsonos-api/src/remote/schedules.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(yes => { resolve = yes; }); return { promise, resolve }; };
const settled = async predicate => {
  for (let n = 0; n < 200 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'expected asynchronous boundary was reached');
};
const row = (change = {}) => ({ id: 7, action: 'dj_start', when: 'weekdays 07:30', zone: 'Studio@S2',
  mood: 'calm', scene: null, volume: null, fade_secs: null, creator: 'Synthetic operator', enabled: true,
  summary: 'Start calm music in Studio', next: '2030-06-17T07:30:00-04:00', last_fired: null, ...change });

function documentFixture() {
  const roots = new Map();
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', type: '', disabled: false, open: false,
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      setAttribute(name, value) { this.attributes[name] = String(value); if (name === 'id' || name === 'type') this[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this._text = ''; this.children = children; },
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

function fixture({ rows = [row()], demo = false, dom = false, network } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Studio', members: ['Studio'], transport_state: 'paused' }));
  const scenes = [{ name: 'Queue for later', saved: 17, volumes: { 'Studio@S2': 20 }, mutes: {},
    groups: [{ coordinator: 'Studio@S2', members: [], playing: true, source: { kind: 'queue_backup',
      queue: { coordinator: 'RINCON_FIXTURE', household: 'S2', update_id: 9, track: 2, position_secs: 30, total: 12, play_mode: 'NORMAL' } } }] }];
  const moods = { moods: [{ name: 'calm', constraints: { energy_bias: -1 } }] }, calls = [];
  let schedules = copy(rows), reader = null, writer = null, time = 1000, control, panel;
  const normalRead = url => {
    if (url === '/schedules') return copy(schedules);
    if (url === '/scenes') return copy(scenes);
    if (url === '/zones') return copy(zones);
    if (url === '/dj/moods') return copy(moods);
    if (url.endsWith('/state')) {
      const zone = zones.find(item => url.includes(encodeURIComponent(C.key(item))));
      return { zone: copy(zone), transport_state: 'paused', volume: 20, track: { uri: 'fixture:song', queue_position: 2 } };
    }
    throw new Error('Unexpected read ' + url);
  };
  const normalWrite = (url, body) => {
    if (url === '/schedules') {
      const when = body.when.startsWith('in ') ? 'once 2030-06-20T19:00:00+09:00'
        : /^\d{4}-/.test(body.when) ? 'once ' + body.when : body.when;
      const result = row({ id: 19, action: body.action, when, zone: body.zone ?? null, mood: body.mood ?? null,
        scene: body.scene ?? null, ...(body.direction !== undefined ? { direction: copy(body.direction) } : {}),
        summary: 'Synthetic saved routine', next: when.startsWith('once ') ? when.slice(5) : '2030-06-18T07:30:00+09:00' });
      schedules.push(copy(result)); return result;
    }
    const action = url.split('/').at(-1), index = schedules.findIndex(item => item.id === body.id), result = copy(schedules[index]);
    if (action === 'remove') schedules.splice(index, 1);
    else { result.enabled = action === 'resume'; result.next = result.enabled ? '2030-06-18T07:30:00+09:00' : null; schedules[index] = copy(result); }
    return result;
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body, options });
    return body === undefined ? reader ? reader(url, options, normalRead) : normalRead(url)
      : writer ? writer(url, body, normalWrite) : normalWrite(url, body);
  };
  const call = network ? network(raw) : raw;
  const ctl = C.model({ call, demo, now: () => time, changed: () => control?.observe() });
  const doc = dom ? documentFixture() : null;
  if (dom) { panel = R.panel({ call, controller: ctl, now: () => time, document: doc }); control = panel.control; }
  else control = R.model({ call, controller: ctl, now: () => time });
  return { control, ctl, panel, doc, calls, zones, scenes, moods, normalRead,
    setRows(value) { schedules = copy(value); }, rows: () => schedules,
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    writes: () => calls.filter(item => item.body !== undefined).map(({ url, body }) => ({ url, body })),
    async ready() { assert.equal(control.open(), true); assert.equal(await control.read(false), true); return this; } };
}
async function creation(f, action = 'apply_scene', timing = {}) {
  assert.equal(await f.control.beginCreate(action), true);
  assert.equal(f.control.edit(action === 'apply_scene' ? 'scene' : 'zone', action === 'apply_scene' ? 'Queue for later' : 'Studio@S1'), true);
  for (const [key, value] of Object.entries(timing)) assert.equal(f.control.edit(key, value), true);
  assert.equal(f.control.reviewCreate(), true); return f.control.state.pending;
}

test('routines read lazily without a selected or online speaker and retain authoritative run timestamps', async () => {
  const f = fixture(); assert.deepEqual(f.calls, []); await f.ready();
  assert.equal(f.ctl.selected(), undefined); assert.equal(f.ctl.canControl(), false); assert.equal(f.ctl.canManage(), true);
  assert.deepEqual(f.calls.map(item => item.url), ['/schedules']);
  assert.equal(f.control.state.rows[0].next, '2030-06-17T07:30:00-04:00'); assert.equal(f.writes().length, 0);
});

test('complete queue-backup scenes can be scheduled with one reviewed native write and no speaker refresh', async () => {
  const f = await fixture().ready(), pending = await creation(f);
  assert.ok(Object.isFrozen(pending)); assert.ok(Object.isFrozen(pending.body));
  assert.match(pending.destination.details, /queue_backup/);
  assert.equal(await f.control.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/schedules', body: { when: 'weekdays 07:30', action: 'apply_scene', scene: 'Queue for later' } }]);
  assert.equal(f.calls.some(item => item.url === '/zones' || item.url.endsWith('/state')), false);
  assert.equal(f.control.state.result.row.next, '2030-06-18T07:30:00+09:00');
  assert.equal(await f.control.apply(pending), false); assert.equal(f.ctl.state.uncertain, false);
});

test('native playlist scenes can be chosen, edited and scheduled from the dashboard routine form', async () => {
  for (const playing of [true, false]) {
    const f = fixture({ dom: true });
    f.scenes.push({ name: 'Native breakfast', saved: 18, volumes: {}, mutes: {},
      groups: [{ coordinator: 'Studio@S1', members: [], playing,
        source: { kind: 'sonos_playlist', name: 'SQ:9',
          playlist: { household: 'SIM_HOUSEHOLD_S1', id: 'SQ:9', title: 'Breakfast & strings' } } }] });
    await f.ready();
    assert.equal(await f.control.beginCreate('apply_scene'), true);
    const chooser = f.doc.getElementById('routine-scene');
    assert.match(chooser.textContent, /Queue for later/);
    assert.match(chooser.textContent, /Native breakfast/);
    chooser.value = 'Native breakfast'; chooser.fire('change');
    assert.equal(f.control.reviewCreate(), true);
    const older = f.control.state.pending;
    assert.equal(JSON.parse(older.destination.details).groups[0].playing, playing);
    assert.match(f.doc.getElementById('routines-body').textContent, /Breakfast & strings/);
    assert.equal(f.control.edit('time', '09:20'), true);
    assert.equal(await f.control.apply(older), false);
    assert.equal(f.control.reviewCreate(), true);
    const reviewed = f.control.state.pending;
    assert.equal(await f.control.apply(reviewed), true);
    assert.equal(await f.control.apply(reviewed), false);
    assert.deepEqual(f.writes(), [{ url: '/schedules', body: {
      when: 'weekdays 09:20', action: 'apply_scene', scene: 'Native breakfast' } }]);
    assert.ok(f.calls.every(call => ['/schedules', '/scenes'].includes(call.url)));
    assert.equal(f.ctl.state.uncertain, false);
  }
});

test('the dashboard rereads a native playlist scene binding before creating its routine', async () => {
  const f = fixture();
  const scene = { name: 'Native evening', saved: 18, volumes: {}, mutes: {},
    groups: [{ coordinator: 'Studio@S2', members: [], playing: true,
      source: { kind: 'sonos_playlist', name: 'SQ:9',
        playlist: { household: 'SIM_HOUSEHOLD_S2', id: 'SQ:9', title: 'Evening strings' } } }] };
  f.scenes.push(scene);
  await f.ready();
  assert.equal(await f.control.beginCreate('apply_scene'), true);
  assert.equal(f.control.edit('scene', scene.name), true);
  assert.equal(f.control.reviewCreate(), true);
  scene.groups[0].source.playlist.household = 'SIM_OTHER_HOUSEHOLD';
  assert.equal(await f.control.apply(), false);
  assert.deepEqual(f.writes(), []);
  assert.match(f.control.state.message, /scene, group or DJ mood changed/);
  assert.equal(f.ctl.state.uncertain, false);
});

test('DJ routines address the reviewed household and preserve filters, mood and relative steering duration', async () => {
  const f = await fixture().ready(); await creation(f, 'dj_start', { cadence: 'delay', delay: '1h30m' });
  f.control.edit('mood', 'calm'); f.control.editDirection('include_artists', 'Synthetic A\nEarth, Wind & Fire');
  f.control.editDirection('minutes', '90'); f.control.reviewCreate();
  assert.equal(await f.control.apply(), true);
  const body = f.writes()[0].body;
  assert.equal(body.zone, 'Studio@S1'); assert.equal(body.when, 'in 1h30m'); assert.equal(body.mood, 'calm');
  assert.deepEqual(body.direction, { constraints: { include_artists: ['Synthetic A', 'Earth, Wind & Fire'] }, for_secs: 5400 });
  assert.equal(f.control.state.result.row.next, '2030-06-20T19:00:00+09:00');
});

test('automatic DJ routines explicitly clear previous steering; custom days and exact offsets use shared validation', async () => {
  for (const [timing, when] of [[{ cadence: 'custom', days: ['fri', 'mon'], time: '19:45' }, 'mon,fri 19:45'],
    [{ cadence: 'once', at: '2030-07-01T18:30:00-05:00' }, '2030-07-01T18:30:00-05:00']]) {
    const f = await fixture().ready(); await creation(f, 'dj_start', timing); assert.equal(await f.control.apply(), true);
    assert.equal(f.writes()[0].body.when, when); assert.deepEqual(f.writes()[0].body.direction, { constraints: {} });
  }
  const f = await fixture().ready(); await creation(f); f.control.edit('cadence', 'once'); f.control.edit('at', '2030-02-30T12:00:00Z');
  assert.equal(f.control.reviewCreate(), false); assert.equal(f.writes().length, 0);
});

test('pause, resume and remove manage exact IDs independently of speaker state', async () => {
  for (const [action, enabled] of [['pause', true], ['resume', false], ['remove', true], ['remove', false]]) {
    const f = await fixture({ rows: [row({ enabled })] }).ready();
    assert.equal(f.control.review(action, 7), true); assert.equal(await f.control.apply(), true);
    assert.deepEqual(f.writes(), [{ url: '/schedules/' + action, body: { id: 7 } }]);
    assert.equal(f.calls.every(item => item.url.startsWith('/schedules')), true);
    assert.equal(f.control.state.result.row.enabled, action === 'remove' ? enabled : action === 'resume');
  }
});

test('definitions, creators, enabled state and removed IDs are checked again before management', async () => {
  for (const rows of [[], [row({ creator: 'Someone else' })], [row({ when: 'daily 09:00' })], [row({ enabled: false })],
    [row({ direction: { constraints: { include_genres: ['jazz'] } } })]]) {
    const f = await fixture().ready(); f.control.review('pause', 7); f.setRows(rows);
    assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('a normal scheduler tick and unrelated speaker events do not invalidate the stored routine definition', async () => {
  const f = await fixture().ready(); f.control.review('pause', 7);
  f.setRows([row({ next: '2030-06-18T07:30:00-04:00', last_fired: '2030-06-17T07:30:00-04:00' })]);
  f.ctl.invalidate(); assert.equal(await f.control.apply(), true); assert.equal(f.writes().length, 1);
});

test('equivalent routines are checked against a fresh list immediately before creation', async () => {
  const f = await fixture().ready(), pending = await creation(f);
  f.setRows([row({ action: 'apply_scene', scene: pending.body.scene, zone: null, mood: null, enabled: false })]);
  assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0); assert.match(f.control.state.message, /equivalent/);
});

test('changed scene content, target membership or mood constraints discard creation before dispatch', async () => {
  for (const change of ['scene', 'group', 'mood']) {
    const f = await fixture().ready(); await creation(f, change === 'scene' ? 'apply_scene' : 'dj_start');
    if (change === 'scene') f.scenes[0].volumes['Studio@S2'] = 80;
    else if (change === 'group') f.zones[1].members.push('New member');
    else { f.control.edit('mood', 'calm'); f.control.reviewCreate(); f.moods.moods[0].constraints.energy_bias = 2; }
    assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0);
  }
});

test('edits retire immutable previews and a retained older confirmation cannot submit a newer draft', async () => {
  const f = await fixture().ready(), pending = await creation(f);
  f.control.edit('time', '10:00'); f.control.reviewCreate();
  assert.equal(await f.control.apply(pending), false); assert.equal(f.writes().length, 0);
  assert.equal(await f.control.apply(), true); assert.equal(f.writes()[0].body.when, 'weekdays 10:00');
});

test('reviews that age out during preflight are retired even without an observe tick', async () => {
  const f = await fixture().ready(); await creation(f);
  f.read((url, options, normal) => { if (url === '/scenes') f.time(1000 + R.MAX_AGE); return normal(url); });
  assert.equal(await f.control.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
});

test('closing, hiding, disposal and controller suspension during preflight suppress unsent writes', async () => {
  for (const action of ['close', 'suspend', 'dispose', 'controller']) {
    const f = await fixture().ready(); f.control.review('pause', 7); const gate = deferred(); let waiting = false;
    f.read(async (url, options, normal) => { waiting = true; await gate.promise; return normal(url); });
    const applying = f.control.apply(); await settled(() => waiting);
    if (action === 'controller') { f.ctl.suspend(); f.ctl.resume(); } else f.control[action]();
    gate.resolve(); assert.equal(await applying, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('late list and option reads cannot restore a closed or superseded panel', async () => {
  const f = fixture(), gate = deferred(); f.read(async (url, options, normal) => { await gate.promise; return normal(url); });
  f.control.open(); const reading = f.control.read(); f.control.close(); gate.resolve(); assert.equal(await reading, false);
  assert.equal(f.control.state.loaded, false); assert.deepEqual(f.control.state.rows, []);
  await f.ready(); const later = deferred(); f.read(async (url, options, normal) => { await later.promise; return normal(url); });
  const choices = f.control.beginCreate('apply_scene'); f.control.close(); later.resolve(); assert.equal(await choices, false);
  assert.equal(f.control.state.draft, null);
});

test('a lost creation reply blocks both command kinds until an explicit fresh routine list is inspected', async () => {
  const f = await fixture().ready(); await creation(f);
  f.write((url, body, normal) => { normal(url, body); throw new Error('Connection lost after storage'); });
  assert.equal(await f.control.apply(), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.control.state.result, null);
  assert.equal(await f.ctl.command('pause'), false); assert.equal(f.ctl.canManage(), false);
  f.control.close(); f.control.open(); await f.control.read(false); assert.equal(f.ctl.state.uncertain, true);
  f.ctl.suspend(); f.ctl.resume(); assert.equal(f.ctl.state.uncertain, true);
  await f.ctl.refresh(true); assert.equal(f.ctl.state.uncertain, true);
  assert.equal(await f.control.read(true), true); assert.equal(f.ctl.state.uncertain, false);
  assert.equal(f.control.state.rows.length, 2); assert.equal(f.writes().length, 1);
});

test('malformed routine acknowledgements never publish success and an unreadable recovery list keeps uncertainty', async () => {
  for (const reply of [null, row({ id: 99 }), row({ enabled: true }), row({ creator: 'Different owner', enabled: false })]) {
    const f = await fixture().ready(); f.control.review('pause', 7); f.write(() => reply);
    assert.equal(await f.control.apply(), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.control.state.result, null);
    f.read(() => ({})); assert.equal(await f.control.read(true), false); assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.writes().length, 1);
  }
});

test('malformed creation receipts must confirm new ID, direction, target and daemon-computed deadline', async () => {
  for (const change of [{ id: 7 }, { enabled: false }, { scene: 'Other' }, { next: null }, { when: 'daily 20:00' }]) {
    const f = await fixture().ready(); await creation(f);
    f.write((url, body, normal) => ({ ...normal(url, body), ...change }));
    assert.equal(await f.control.apply(), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.control.state.result, null);
  }
});

test('speaker uncertainty cannot be acknowledged by a schedule-list read', async () => {
  const f = await fixture().ready(); await f.ctl.refresh();
  f.write(() => { throw new Error('Speaker reply lost'); });
  assert.equal(await f.ctl.command('pause'), false); assert.equal(f.ctl.state.uncertain, true);
  assert.equal(await f.control.read(true), true); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.ctl.canManage(), false);
  assert.match(f.control.state.message, /speaker command/);
  await f.ctl.refresh(true); assert.equal(f.ctl.state.uncertain, false);
});

test('stored and speaker commands share one busy lane, including after the routine dialog closes', async () => {
  const f = await fixture().ready(); await f.ctl.refresh(); f.control.review('pause', 7);
  const gate = deferred(); let sent = false;
  f.write(async (url, body, normal) => { const value = normal(url, body); sent = true; await gate.promise; return value; });
  const applying = f.control.apply(); await settled(() => sent); f.control.close();
  assert.equal(f.ctl.state.busy, true); assert.equal(await f.ctl.command('pause'), false); assert.equal(f.control.open(), false);
  gate.resolve(); assert.equal(await applying, true); assert.equal(f.ctl.state.busy, false); assert.equal(f.writes().length, 1);
  assert.equal(f.control.state.result, null);
});

test('stored lane rejects speaker endpoints and unknown storage scopes without sending', async () => {
  const f = fixture();
  assert.equal(await f.ctl.mutate(({ send }) => send('/pause', { zone: 'Studio@S2' }), { stored: 'schedules' }), false);
  assert.equal(await f.ctl.mutate(({ send }) => send('/schedules/pause', { id: 7 }, { method: 'PUT' }), { stored: 'schedules' }), false);
  assert.equal(await f.ctl.mutate(() => ({ done: 'No', changed: true }), { stored: 'other' }), false);
  assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
});

test('preview mode and malformed catalogs cannot authorize a stored write', async () => {
  const demo = await fixture({ demo: true }).ready(); assert.equal(await demo.control.beginCreate('dj_start'), false);
  assert.equal(demo.control.review('pause', 7), false); assert.equal(demo.ctl.canManage(), false);
  const f = await fixture().ready(); f.read((url, options, normal) => url === '/scenes' ? [{}] : normal(url));
  assert.equal(await f.control.beginCreate('apply_scene'), false); assert.equal(f.control.reviewCreate(), false); assert.equal(f.writes().length, 0);
});

test('actual dialog handlers schedule a scene, show reported timing and pause its exact ID after refresh', async () => {
  const f = fixture({ dom: true }), get = id => f.doc.getElementById(id);
  const click = key => {
    const button = get('routines-body').querySelectorAll('[data-routine-focus]').find(node => node.getAttribute('data-routine-focus') === key);
    assert.ok(button, key); button.fire('click');
  };
  get('open-routines').fire('click'); await settled(() => f.control.state.loaded);
  assert.equal(get('routines-dialog').open, true); assert.match(get('routines-body').textContent, /house reports 2030-06-17T07:30:00-04:00/);
  get('routines-create-scene').fire('click'); await settled(() => f.control.state.draft);
  get('routine-scene').value = 'Queue for later'; get('routine-scene').fire('change');
  get('routine-time').value = '21:15'; get('routine-time').fire('input'); click('review-create');
  assert.match(get('routines-body').textContent, /daemon host’s local timezone/); assert.equal(f.writes().length, 0);
  click('confirm'); await settled(() => f.control.state.result);
  assert.match(get('routines-body').textContent, /2030-06-18T07:30:00\+09:00/); assert.equal(f.writes().length, 1);
  get('routines-read').fire('click'); await settled(() => f.control.state.loaded); click('toggle-19'); click('confirm');
  await settled(() => f.control.state.result); assert.deepEqual(f.writes()[1], { url: '/schedules/pause', body: { id: 19 } });
  get('routines-dialog').fire('cancel'); assert.equal(f.control.state.open, false); assert.equal(get('routines-dialog').open, false);
});

test('the production DJ direction editor preserves typed fields and consumes an older confirmation', async () => {
  const f = await fixture({ dom: true }).ready(), get = id => f.doc.getElementById(id);
  await creation(f, 'dj_start'); f.control.cancel(); await f.control.beginCreate('dj_start');
  get('routine-zone').value = 'Studio@S2'; get('routine-zone').fire('change');
  let artist = get('routine-direction-include_artists'); artist.value = 'Earth, Wind & Fire'; artist.selectionStart = 6; artist.focus(); artist.fire('input');
  artist = get('routine-direction-include_artists'); assert.equal(f.doc.activeElement, artist); assert.equal(artist.selectionStart, 6);
  assert.equal(artist.value, 'Earth, Wind & Fire'); assert.equal(f.control.reviewCreate(), true);
  const pending = f.control.state.pending; f.control.editDirection('minutes', '60'); assert.equal(await f.control.apply(pending), false);
  assert.equal(f.control.reviewCreate(), true); assert.equal(await f.control.apply(), true);
  assert.equal(f.writes()[0].body.direction.for_secs, 3600);
});

test('the shared scheduler helpers load in a dashboard without full-remote globals or mounting a second writer', () => {
  const source = fs.readFileSync(require.resolve('../crates/fsonos-api/src/remote/schedules.js'), 'utf8');
  const context = vm.createContext({}); vm.runInContext(source, context);
  assert.equal(vm.runInContext('typeof SchedulesRemote.creation', context), 'function');
});

test('real loopback HTTP sends the reviewed native routine body once while all speakers are unavailable', async t => {
  let backend; const received = [];
  const server = http.createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : undefined; received.push({ url: req.url, method: req.method, body });
    const value = await backend(req.url, body); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const f = await fixture({ network(raw) { backend = raw;
    return C.request({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) }); } }).ready();
  await creation(f, 'apply_scene', { cadence: 'weekends', time: '09:30' }); assert.equal(await f.control.apply(), true);
  assert.deepEqual(received.filter(item => item.method === 'POST'), [{ url: '/schedules', method: 'POST',
    body: { when: 'weekends 09:30', action: 'apply_scene', scene: 'Queue for later' } }]);
  assert.equal(received.some(item => item.url.includes('/state')), false);
});
