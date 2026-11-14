'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const M = require('../crates/fsonos-api/src/remote/room-configuration.js');
const P = require('../crates/fsonos-api/src/remote/room-configuration-panel.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Studio = require('../crates/fsonos-api/src/remote/studio.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(yes => { resolve = yes; }); return { promise, resolve }; };
async function settled(predicate) {
  for (let n = 0; n < 300 && !predicate(); n++) await new Promise(setImmediate);
  assert.ok(predicate(), 'expected asynchronous boundary was reached');
}
function configuration() {
  return { revision: 'opaque-room-config-A', aliases: [
    { name: 'Owner’s quiet set', rooms: [' Study@SIM_S2 ', 'Offline cabin@SIM_S1', 'Offline cabin@SIM_S1'] },
    { name: 'Morning', rooms: ['Hall@SIM_S2'] },
  ], default_room: 'Study@SIM_S2', client_defaults: Object.fromEntries([
    ['Unknown/Client 🌙', ' Offline target@SIM_S1 '], ['__proto__', 'Hall@SIM_S2'], ['MixedCASE', 'Study@SIM_S1'],
  ]), warnings: [{ line: 12, message: 'Synthetic cabin is currently offline.' }] };
}
function documentFixture() {
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', type: '', disabled: false, readOnly: false, open: false,
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      set innerHTML(_) { throw new Error('Configuration metadata must remain text'); },
      setAttribute(name, value) { this.attributes[name] = String(value); if (name === 'id' || name === 'type') this[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this._text = ''; this.children = children; },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(node => node.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; }, setSelectionRange(start, end = start) { this.selectionStart = start; this.selectionEnd = end; } };
  } };
  return doc;
}
function fixture(surface, { snapshot = configuration(), dom = false, demo = false, network } = {}) {
  let config = copy(snapshot), reader = null, writer = null, control;
  const rooms = [
    { name: 'Study', household: 'SIM_S2', zone: 'Study', aliases: ['Owner’s quiet set'] },
    { name: 'Hall', household: 'SIM_S2', zone: 'Study' },
    { name: 'Study', household: 'SIM_S1', zone: 'Study', aliases: [] },
  ], calls = [];
  const read = url => {
    if (url === '/rooms/configuration') return copy(config);
    if (url === '/rooms') return copy(rooms);
    if (url === '/zones' || url === '/schedules') return [];
    throw new Error('Unexpected fixture read: ' + url);
  };
  const write = (url, body) => {
    assert.equal(url, '/rooms/configuration'); assert.equal(body.expected_revision, config.revision);
    const before = JSON.stringify([config.aliases, config.default_room, config.client_defaults]), change = body.change;
    switch (change.kind) {
      case 'set_alias': {
        const index = config.aliases.findIndex(alias => alias.name === change.name);
        if (index < 0) config.aliases.push({ name: change.name, rooms: copy(change.rooms) });
        else config.aliases[index].rooms = copy(change.rooms);
        break;
      }
      case 'remove_alias': config.aliases = config.aliases.filter(alias => alias.name !== change.name); break;
      case 'set_default':
        if (Object.hasOwn(change, 'client')) config.client_defaults = { ...config.client_defaults, [change.client]: change.room };
        else config.default_room = change.room;
        break;
      case 'clear_default':
        if (Object.hasOwn(change, 'client')) config.client_defaults = Object.fromEntries(Object.entries(config.client_defaults).filter(([key]) => key !== change.client));
        else config.default_room = null;
        break;
      default: throw new Error('Unexpected change kind');
    }
    const changed = before !== JSON.stringify([config.aliases, config.default_room, config.client_defaults]);
    if (changed) config.revision += '.next';
    return { done: changed ? 'Saved synthetic room configuration.' : 'Configuration already matched.', changed, configuration: copy(config) };
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, options, read) : read(url)
      : writer ? writer(url, body, write) : write(url, body);
  };
  const call = network ? network(raw) : raw;
  const controller = surface === 'dashboard' ? C.model({ call, demo, changed: () => control?.observe() }) : null;
  const options = controller ? { call, canEdit: () => controller.canManage(), canRead: () => controller.state.active && !controller.state.busy,
    blockedReason: () => demo ? 'Design preview is read-only.' : controller.state.message,
    readConfiguration: (decode, opts) => controller.refreshStored('roomConfiguration', decode, opts),
    execute: operation => controller.mutate(operation, { stored: 'roomConfiguration' }) } : { call, canEdit: () => !demo };
  const document = dom ? documentFixture() : null, panel = dom ? P.panel({ ...options, document }) : null;
  control = panel ? panel.control : M.model(options);
  return { surface, control, controller, panel, document, rooms, calls, call,
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, setConfig(value) { config = copy(value); }, config: () => copy(config),
    writes: () => calls.filter(call => call.body !== undefined),
    async ready() { assert.equal(control.open(), true); await settled(() => !control.state.loading); assert.equal(control.state.loaded, true, control.state.message); return this; } };
}
function addRoom(control, room) { assert.equal(control.edit('room_input', room), true); assert.equal(control.addRoom(), true, control.state.message); }
function newAlias(f, name = 'Evening fixture', rooms = ['Study@SIM_S2', 'Offline attic@SIM_S1']) {
  assert.equal(f.control.beginAlias(), true); f.control.edit('name', name);
  for (const room of rooms) addRoom(f.control, room);
  const pending = f.control.review(); assert.ok(pending, f.control.state.message); return pending;
}

for (const surface of ['phone', 'dashboard']) {
  test(`${surface}: reads complete aliases/defaults lazily without a speaker or an immediate write`, async () => {
    const f = fixture(surface); assert.deepEqual(f.calls, []); await f.ready();
    assert.deepEqual(f.control.state.configuration.client_defaults, configuration().client_defaults);
    assert.deepEqual(f.control.state.configuration.aliases[0].rooms, configuration().aliases[0].rooms);
    assert.deepEqual(f.control.state.rooms.map(room => room.target), ['Study@SIM_S2', 'Hall@SIM_S2', 'Study@SIM_S1']);
    assert.ok(Object.isFrozen(f.control.state.configuration)); assert.ok(Object.isFrozen(f.control.state.configuration.aliases[0].rooms));
    assert.deepEqual(f.calls.map(call => call.url).sort(), ['/rooms', '/rooms/configuration']);
    assert.equal(f.writes().length, 0); if (f.controller) assert.equal(f.controller.canControl(), false);
  });

  test(`${surface}: creates an alias through one reviewed native write while preserving unknown client keys`, async () => {
    const f = await fixture(surface).ready(), pending = newAlias(f);
    assert.deepEqual(pending.body, { expected_revision: 'opaque-room-config-A', change: {
      kind: 'set_alias', name: 'Evening fixture', rooms: ['Study@SIM_S2', 'Offline attic@SIM_S1'] } });
    assert.ok(Object.isFrozen(pending.body.change.rooms)); assert.equal(f.writes().length, 0);
    assert.equal(await f.control.save(pending), true); assert.equal(await f.control.save(pending), false);
    assert.deepEqual(f.config().client_defaults, configuration().client_defaults);
    assert.deepEqual(f.config().aliases.slice(0, 2), configuration().aliases);
    assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].url, '/rooms/configuration');
    assert.equal(f.control.state.result.status, 'accepted'); assert.equal(f.control.state.draft, null);
    assert.ok(f.calls.every(call => ['/rooms', '/rooms/configuration'].includes(call.url)));
  });

  test(`${surface}: edits the exact alias identity, retaining offline strings, order, whitespace and repeated rooms`, async () => {
    const f = await fixture(surface).ready(); assert.equal(f.control.beginAlias('Owner’s quiet set'), true);
    assert.equal(f.control.edit('name', 'Accidental rename'), false); addRoom(f.control, 'Hall@SIM_S2');
    const pending = f.control.review(); assert.equal(await f.control.save(pending), true);
    assert.equal(pending.body.change.name, 'Owner’s quiet set');
    assert.deepEqual(f.config().aliases[0].rooms, [...configuration().aliases[0].rooms, 'Hall@SIM_S2']);
    assert.equal(f.config().default_room, configuration().default_room); assert.equal(f.writes().length, 1);
  });

  test(`${surface}: removes a saved alias without removing defaults or changing playback`, async () => {
    const f = await fixture(surface).ready(); f.control.beginAlias('Morning', true); const pending = f.control.review();
    assert.match(pending.summary, /Future commands and routines/); assert.equal(await f.control.save(pending), true);
    assert.deepEqual(pending.body.change, { kind: 'remove_alias', name: 'Morning' });
    assert.deepEqual(f.config().aliases, [configuration().aliases[0]]); assert.deepEqual(f.config().client_defaults, configuration().client_defaults);
  });

  for (const [label, client, clear, room] of [
    ['global set', null, false, 'Hall@SIM_S2'], ['global clear', null, true, null],
    ['unknown client edit', 'Unknown/Client 🌙', false, ' Offline loft@SIM_S1 '],
    ['unknown client clear', 'Unknown/Client 🌙', true, null],
    ['prototype-shaped key edit', '__proto__', false, 'Study@SIM_S1'],
  ]) test(`${surface}: ${label} preserves all other saved settings and exact client identities`, async () => {
    const f = await fixture(surface).ready(); assert.equal(f.control.beginDefault(client, clear), true);
    if (!clear) assert.equal(f.control.edit('room', room), true);
    if (client !== null) assert.equal(f.control.edit('client', 'retargeted'), false);
    const pending = f.control.review(); assert.ok(pending);
    assert.deepEqual(pending.body.change, { kind: clear ? 'clear_default' : 'set_default', ...(client === null ? {} : { client }), ...(clear ? {} : { room }) });
    assert.equal(await f.control.save(pending), true); assert.deepEqual(f.config().aliases, configuration().aliases);
    if (client === null) assert.deepEqual(f.config().client_defaults, configuration().client_defaults);
    else {
      assert.equal(f.config().default_room, configuration().default_room);
      for (const [key, value] of Object.entries(configuration().client_defaults)) if (key !== client) assert.equal(f.config().client_defaults[key], value);
      if (clear) assert.equal(Object.hasOwn(f.config().client_defaults, client), false);
      else assert.equal(f.config().client_defaults[client], room);
    }
    assert.equal(f.writes().length, 1);
  });

  test(`${surface}: creates an individual default with the explicitly entered client key`, async () => {
    const f = await fixture(surface).ready(); f.control.beginDefault('');
    f.control.edit('client', 'NewClient/Exact CASE'); f.control.edit('room', 'Offline study@SIM_S1');
    const pending = f.control.review(); assert.equal(await f.control.save(pending), true);
    assert.equal(f.config().client_defaults['NewClient/Exact CASE'], 'Offline study@SIM_S1');
    assert.equal(f.config().client_defaults['newclient/exact case'], undefined);
  });

  test(`${surface}: an unchanged alias/default is verified as a no-op with the same revision`, async () => {
    for (const select of [control => control.beginAlias('Owner’s quiet set'), control => control.beginDefault(null), control => control.beginDefault('MixedCASE')]) {
      const f = await fixture(surface).ready(); select(f.control); const pending = f.control.review();
      assert.equal(await f.control.save(pending), true); assert.equal(f.control.state.result.receipt.changed, false);
      assert.equal(f.config().revision, 'opaque-room-config-A'); assert.equal(f.writes().length, 1);
    }
  });

  test(`${surface}: every draft change consumes its old review and keeps the original configuration revision`, async () => {
    const f = await fixture(surface).ready(), prior = newAlias(f);
    f.control.removeRoom(1); assert.equal(await f.control.save(prior), false);
    const pending = f.control.review(); assert.equal(pending.body.expected_revision, prior.body.expected_revision);
    assert.deepEqual(prior.body.change.rooms, ['Study@SIM_S2', 'Offline attic@SIM_S1']);
    assert.deepEqual(pending.body.change.rooms, ['Study@SIM_S2']); assert.equal(await f.control.save(pending), true);
    assert.equal(f.writes().length, 1);
  });

  test(`${surface}: stale revision or inconsistent preflight data sends no POST and never rebases the draft`, async () => {
    for (const mutate of [value => ({ ...value, revision: 'outside-edit' }), value => ({ ...value, default_room: 'Hall@SIM_S2' })]) {
      const f = await fixture(surface).ready(), pending = newAlias(f); f.setConfig(mutate(f.config()));
      assert.equal(await f.control.save(pending), false); assert.equal(f.control.state.result.status, 'not_sent');
      assert.equal(f.writes().length, 0); assert.equal(f.control.state.draft.name, 'Evening fixture');
      await f.control.load(); assert.equal(f.control.state.stale, true); assert.equal(f.control.review(), null);
      assert.equal(f.control.state.base.revision, 'opaque-room-config-A'); assert.equal(f.writes().length, 0);
    }
  });

  test(`${surface}: a race reported as CONFIG_CHANGED never retries or silently refreshes the write`, async () => {
    const f = await fixture(surface).ready(), pending = newAlias(f);
    f.write(() => { throw Object.assign(new Error('Synthetic stale configuration'), { code: 'CONFIG_CHANGED', status: 409 }); });
    assert.equal(await f.control.save(pending), false); assert.equal(f.control.state.result.status, 'conflict');
    assert.equal(await f.control.save(pending), false); assert.equal(f.writes().length, 1);
    assert.equal(f.control.state.draft.name, 'Evening fixture'); await f.control.load({ explicit: true });
    assert.equal(f.control.state.result.status, 'observed'); assert.equal(f.writes().length, 1);
  });

  test(`${surface}: a lost reply retains its draft and requires an explicit configuration observation`, async () => {
    const f = await fixture(surface).ready(), pending = newAlias(f);
    f.write((url, body, normal) => { normal(url, body); throw new Error('Synthetic acknowledgement lost'); });
    assert.equal(await f.control.save(pending), false); assert.equal(f.control.state.result.status, 'uncertain');
    assert.equal(f.control.state.draft.name, 'Evening fixture'); assert.equal(await f.control.save(pending), false);
    if (f.controller) {
      assert.equal(f.controller.state.uncertain, true); await f.controller.refresh(true);
      await f.controller.refreshStored('schedules', value => value, { explicit: true }); assert.equal(f.controller.state.uncertain, true);
    }
    await f.control.load({ explicit: false }); assert.equal(f.control.state.result.status, 'uncertain'); assert.equal(f.control.beginAlias(), false);
    await f.control.load({ explicit: true }); assert.equal(f.control.state.result.status, 'observed');
    assert.match(f.control.state.result.detail, /does not prove/); assert.equal(f.control.state.stale, true);
    assert.equal(f.control.review(), null); assert.equal(f.control.state.draft.name, 'Evening fixture'); assert.equal(f.writes().length, 1);
    if (f.controller) assert.equal(f.controller.state.uncertain, false);
  });

  test(`${surface}: malformed receipts or changed unrelated settings remain uncertain with no second write`, async () => {
    for (const mutate of [answer => ({ ...answer, changed: false }), answer => ({ ...answer, configuration: { ...answer.configuration, revision: 'opaque-room-config-A' } }),
      answer => ({ ...answer, configuration: { ...answer.configuration, client_defaults: {} } })]) {
      const f = await fixture(surface).ready(), pending = newAlias(f);
      f.write((url, body, normal) => mutate(normal(url, body)));
      assert.equal(await f.control.save(pending), false); assert.equal(f.control.state.result.status, 'uncertain');
      assert.equal(f.control.state.draft.name, 'Evening fixture'); assert.equal(await f.control.save(pending), false); assert.equal(f.writes().length, 1);
    }
  });

  test(`${surface}: an unreadable recovery snapshot cannot acknowledge an uncertain command`, async () => {
    const f = await fixture(surface).ready(), pending = newAlias(f);
    f.write(() => { throw new Error('Lost reply'); }); await f.control.save(pending);
    f.read((url, options, normal) => url === '/rooms/configuration' ? { ...normal(url), client_defaults: [] } : normal(url));
    assert.equal(await f.control.load(), false); assert.equal(f.control.state.result.status, 'uncertain'); assert.ok(f.control.state.recovery);
    if (f.controller) assert.equal(f.controller.state.uncertain, true);
  });

  test(`${surface}: closing or suspending before the POST fences late preflight without discarding the draft`, async () => {
    for (const leave of [control => control.close(), control => control.suspend()]) {
      const f = await fixture(surface).ready(), pending = newAlias(f), gate = deferred();
      f.read(async (url, options, normal) => { if (url === '/rooms/configuration') await gate.promise; return normal(url); });
      const saving = f.control.save(pending); leave(f.control); gate.resolve();
      assert.equal(await saving, false); assert.equal(f.writes().length, 0); assert.equal(f.control.state.draft.name, 'Evening fixture');
    }
  });

  test(`${surface}: a sent write stays single-flight after closing and its acceptance is retained`, async () => {
    const f = await fixture(surface).ready(), pending = newAlias(f), gate = deferred();
    f.write(async (url, body, normal) => { await gate.promise; return normal(url, body); });
    const saving = f.control.save(pending); await settled(() => f.writes().length === 1); f.control.close();
    assert.equal(await f.control.save(pending), false); assert.equal(await f.control.load(), false); assert.equal(f.control.open(), false);
    if (f.controller) assert.equal(await f.controller.mutate(() => { throw new Error('Must remain blocked'); }, { stored: 'schedules' }), false);
    gate.resolve(); assert.equal(await saving, true); assert.equal(f.control.state.result.status, 'accepted'); assert.equal(f.writes().length, 1);
    assert.equal(f.control.open(), true); await settled(() => !f.control.state.loading); assert.equal(f.control.state.result.status, 'accepted');
  });

  test(`${surface}: absent room discovery does not prevent retaining or authoring exact offline targets`, async () => {
    const f = fixture(surface); f.read((url, options, normal) => { if (url === '/rooms') throw new Error('No room inventory'); return normal(url); });
    await f.ready(); assert.deepEqual(f.control.state.rooms, []); assert.match(f.control.state.roomMessage, /exact physical room names/);
    const pending = newAlias(f, 'Offline fixture', ['Offline room@SIM_S1']); assert.equal(await f.control.save(pending), true);
    assert.deepEqual(f.config().aliases.at(-1).rooms, ['Offline room@SIM_S1']);
  });
}

test('validation matches native reserved names, Unicode bounds and exact-client semantics without normalizing stored strings', async () => {
  const f = await fixture('phone').ready();
  for (const name of ['here', 'All@SIM_S2', 'everywhere', '', 'x\nname', 'a'.repeat(129), "OWNER'S QUIET SET"]) {
    f.control.beginAlias(); f.control.edit('name', name); addRoom(f.control, 'Study@SIM_S2'); assert.equal(f.control.review(), null, name);
  }
  f.control.beginAlias(); f.control.edit('name', 'valid');
  for (const room of ['all@SIM_S1', 'HERE', 'everywhere@SIM_S2', '', 'Room\u0000', 'a'.repeat(129)]) {
    f.control.edit('room_input', room); assert.equal(f.control.addRoom(), false, room);
  }
  f.control.edit('room_input', 'Study@SIM_S2'); assert.equal(f.control.review(), null, 'typed room must be deliberately added');
  f.control.addRoom(); assert.ok(f.control.review());
  for (const room of ['here', 'all@SIM_S1', 'everywhere@SIM_S2', 'a'.repeat(129)]) {
    f.control.beginDefault(null); f.control.edit('room', room); assert.equal(f.control.review(), null, room);
  }
  f.control.beginDefault(''); f.control.edit('client', '🌙'.repeat(129)); f.control.edit('room', 'Study@SIM_S2'); assert.equal(f.control.review(), null);
  f.control.edit('client', '🌙'.repeat(128)); assert.ok(f.control.review()); assert.equal(f.writes().length, 0);
});

test('complete configuration decoding fails closed on conflicting identities and unknown fields', () => {
  for (const change of [{ revision: '' }, { revision: 'bad\nrevision' }, { default_room: undefined }, { client_defaults: [] },
    { aliases: [{ name: 'One', rooms: [] }] }, { aliases: [{ name: 'Owner’s', rooms: ['A'] }, { name: "OWNER'S", rooms: ['B'] }] },
    { warnings: [{ line: -1, message: 'bad' }] }, { future_setting: 'must not be discarded' }]) {
    assert.throws(() => M.configurationOf({ ...configuration(), ...change }));
  }
  const unavailable = M.configurationOf({ ...configuration(), warnings: [{ line: 0, message: 'Inventory is unavailable.' }] });
  assert.equal(unavailable.warnings[0].line, 0);
  assert.equal(M.configurationOf(configuration()).client_defaults.__proto__, 'Hall@SIM_S2');
});

test('alias identity follows Rust per-character lowercase and Unicode whitespace, including sigma and retained BOM', async () => {
  assert.equal(M.normalized(' ΟΣ '), 'οσ');
  assert.equal(M.normalized('\u0085ΟΣ\u0085'), 'οσ');
  assert.equal(M.normalized('\ufeffΟΣ\ufeff'), '\ufeffοσ\ufeff');
  assert.notEqual(M.normalized('ΟΣ'), M.normalized('ος'), 'final sigma is a distinct saved identity');
  const snapshot = configuration(); snapshot.aliases.push({ name: 'ΟΣ', rooms: ['Study@SIM_S2'] });
  for (const surface of ['phone', 'dashboard']) {
    const f = await fixture(surface, { snapshot }).ready(); f.control.beginAlias();
    f.control.edit('name', 'οσ'); addRoom(f.control, 'Hall@SIM_S2');
    assert.equal(f.control.review(), null, 'creation cannot overwrite a native normalized match');
    assert.match(f.control.state.message, /already exists/); assert.equal(f.writes().length, 0);
    assert.deepEqual(f.config().aliases.at(-1), { name: 'ΟΣ', rooms: ['Study@SIM_S2'] });
  }
  const distinct = { ...configuration(), aliases: [
    { name: 'ΟΣ', rooms: ['Study@SIM_S2'] }, { name: 'ος', rooms: ['Study@SIM_S1'] },
    { name: '\ufeffBoundary', rooms: ['Study@SIM_S2'] }, { name: 'Boundary', rooms: ['Hall@SIM_S2'] },
  ] };
  assert.deepEqual(M.configurationOf(distinct).aliases, distinct.aliases);
});

test('receipt comparison accepts reordered read metadata but rejects loss of repeated members or unknown client defaults', async () => {
  const f = await fixture('phone').ready(), planned = newAlias(f);
  const good = { done: 'Saved.', changed: true, configuration: { ...copy(planned.expected), revision: 'new',
    aliases: [...copy(planned.expected.aliases)].reverse(), warnings: [{ line: 0, message: 'Inventory unavailable after save.' }] } };
  assert.equal(M.receiptOf(good, planned).changed, true);
  const bad = copy(good); bad.configuration.aliases.find(alias => alias.name === 'Owner’s quiet set').rooms.pop();
  assert.throws(() => M.receiptOf(bad, planned), /exactly/);
  const clients = copy(good); delete clients.configuration.client_defaults['Unknown/Client 🌙'];
  assert.throws(() => M.receiptOf(clients, planned), /exactly/);
});

test('dashboard stored configuration lane permits only its own native endpoint and demo mode cannot save', async () => {
  const f = await fixture('dashboard').ready();
  assert.equal(await f.controller.mutate(({ send }) => send('/pause', { zone: 'all' }), { stored: 'roomConfiguration' }), false);
  assert.equal(await f.controller.mutate(({ send }) => send('/rooms/configuration/other', {}), { stored: 'roomConfiguration' }), false);
  assert.equal(f.writes().length, 0);
  const preview = await fixture('dashboard', { demo: true }).ready(); assert.equal(preview.control.beginAlias(), false); assert.equal(preview.writes().length, 0);
});

test('a configuration read cannot clear a different stored-command uncertainty and explains the required recovery', async () => {
  const f = await fixture('dashboard').ready();
  f.write(() => { throw new Error('Synthetic routine reply lost'); });
  await f.controller.mutate(async ({ send }) => send('/schedules/pause', { id: 9 }), { stored: 'schedules' });
  assert.equal(f.controller.state.uncertain, true); assert.equal(await f.control.load(), true);
  assert.equal(f.controller.state.uncertain, true); assert.equal(f.control.beginAlias(), false);
  assert.match(f.control.state.message, /Refresh routines/);
});

test('repeating an executor callback cannot dispatch a second configuration write', async () => {
  const f = fixture('phone');
  const control = M.model({ call: f.call, execute: async operation => {
    await operation(); await assert.rejects(operation(), /one command/); return true;
  } });
  control.open(); await settled(() => control.state.loaded && !control.state.loading);
  const pending = newAlias({ control }); assert.equal(await control.save(pending), true); assert.equal(f.writes().length, 1);
});

test('shared phone/dashboard DOM edits aliases and defaults with plain text, preserves focus and explains future effects', async () => {
  for (const surface of ['phone', 'dashboard']) {
    const snapshot = configuration(); snapshot.aliases[0].name = '<img src=x onerror=fixture>'; const f = await fixture(surface, { dom: true, snapshot }).ready();
    const find = key => f.panel.node.querySelectorAll('[data-room-config-focus]').find(node => node.getAttribute('data-room-config-focus') === key);
    assert.match(f.panel.node.textContent, /Unknown\/Client 🌙/); assert.match(f.panel.node.textContent, /<img src=x onerror=fixture>/);
    find('edit-alias-0').fire('click'); assert.equal(find('name').readOnly, true);
    const input = find('room_input'); input.focus(); input.value = 'Hall@SIM_S2'; input.setSelectionRange(4); input.fire('input');
    assert.equal(f.document.activeElement.getAttribute('data-room-config-focus'), 'room_input'); assert.equal(f.document.activeElement.selectionStart, 4);
    find('add-room').fire('click'); find('review').fire('click');
    assert.match(f.panel.node.textContent, /scheduled routines/); assert.match(f.panel.node.textContent, /does not start music/);
    assert.match(f.panel.node.textContent, /Offline cabin@SIM_S1/); find('confirm').fire('click');
    await settled(() => f.control.state.result?.status === 'accepted'); assert.equal(f.writes().length, 1);
    find('refresh').fire('click'); await settled(() => !f.control.state.loading && f.control.state.loaded);
    find('clear-default-0').fire('click'); find('review').fire('click');
    assert.match(f.panel.node.textContent, /fall back to the house default/);
    find('confirm').fire('click'); await settled(() => f.writes().length === 2 && !f.control.state.writing);
    assert.deepEqual(f.writes()[1].body.change, { kind: 'clear_default', client: 'Unknown/Client 🌙' });
  }
});

test('studio finds room configuration in House and sound without mistaking it for immediate room-set controls', () => {
  const node = { id: 'room-configuration', getAttribute: () => 'Aliases and default rooms', querySelector: () => null };
  assert.equal(Studio.identify(node).id, 'room-configuration'); assert.equal(Studio.identify(node).workspace, 'house');
  assert.ok(Studio.search('default client', Studio.tools).some(tool => tool.id === 'room-configuration'));
});

test('real loopback HTTP executes the revision-bound alias and default routes with no playback requests', async () => {
  let handler;
  const server = http.createServer(async (request, response) => {
    try {
      let raw = ''; for await (const part of request) raw += part;
      const body = raw ? JSON.parse(raw) : undefined;
      assert.equal(request.method, body === undefined ? 'GET' : 'POST');
      const value = await handler(request.url, body); response.setHeader('content-type', 'application/json'); response.end(JSON.stringify(value));
    } catch (error) { response.statusCode = 500; response.end(JSON.stringify({ message: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const base = `http://127.0.0.1:${server.address().port}`;
    const f = await fixture('dashboard', { network: raw => {
      handler = raw; return async (url, body, options) => {
        const response = await fetch(base + url, { method: body === undefined ? 'GET' : 'POST', headers: { 'content-type': 'application/json' },
          ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: options?.signal });
        const result = await response.json(); if (!response.ok) throw new Error(result.message); return result;
      };
    } }).ready();
    let pending = newAlias(f); assert.equal(await f.control.save(pending), true);
    await f.control.load(); f.control.beginDefault('Unknown/Client 🌙'); f.control.edit('room', 'Study@SIM_S1');
    pending = f.control.review(); assert.equal(await f.control.save(pending), true);
    assert.deepEqual(f.writes().map(call => call.url), ['/rooms/configuration', '/rooms/configuration']);
    assert.equal(f.writes()[1].body.expected_revision, 'opaque-room-config-A.next');
    assert.equal(f.controller.state.uncertain, false); assert.ok(f.calls.every(call => ['/rooms', '/rooms/configuration'].includes(call.url)));
  } finally { await new Promise(resolve => server.close(resolve)); }
});
