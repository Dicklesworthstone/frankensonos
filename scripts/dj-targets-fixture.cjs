'use strict';
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const T = require('../crates/fsonos-api/src/remote/dj-targets.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const copy = value => structuredClone(value);
const target = 'Evening & upstairs/#';
const defer = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
async function until(check) {
  for (let i = 0; i < 400 && !check(); i++) await Promise.resolve();
  assert.ok(check(), 'expected asynchronous boundary');
}
const work = title => ({ title, composer: 'Fixture composer', performers: ['Earth, Wind & Fire'], album: 'Fixture album',
  movements: 3, movement: 1, reason: { summary: 'A whole-work fixture', factors: [], relaxed: [] } });
function fixture({ dashboard = false, changed = () => {} } = {}) {
  const room = (household, name, coordinator = name) => ({ household, room: name,
    player: `${household}-${name}`, coordinator: `${household}-${coordinator}` });
  const rooms = [room('S1', 'Den'), room('S1', 'Side', 'Den'), room('S1', 'Patio', 'Den'), room('S2', 'Den'), room('S2', 'Kitchen')];
  const groups = [
    { household: 'S1', coordinator: 'S1-Den', coordinator_room: 'Den', members: rooms.slice(0, 3) },
    { household: 'S2', coordinator: 'S2-Den', coordinator_room: 'Den', members: [rooms[3]] },
    { household: 'S2', coordinator: 'S2-Kitchen', coordinator_room: 'Kitchen', members: [rooms[4]] },
  ];
  const h = { groups, selected: [rooms[0], rooms[1], rooms[3], rooms[4]], calls: [], time: 0, executions: 0,
    custom: null, reply: null, execute: null, completed: [], moods: { moods: [
      { name: 'focus', constraints: { include_genres: ['classical'] } }, { name: 'calm', constraints: { energy_bias: -1 } },
    ] }, addressRevision: 0 };
  h.zones = () => h.groups.map(group => ({ household: group.household, coordinator_room: group.coordinator_room,
    members: group.members.map(room => room.room), transport_state: 'paused' }));
  h.states = new Map(h.zones().map(zone => [`${zone.coordinator_room}@${zone.household}`,
    { zone, transport_state: 'paused', volume: 20, mute: false,
      track: { uri: 'fixture:current', title: 'Current <music>', queue_position: 2, position_secs: 10, duration_secs: 300 } }]));
  h.statuses = new Map(h.zones().map((zone, index) => [`${zone.coordinator_room}@${zone.household}`, {
    zone: zone.coordinator_room, running: index !== 2, now: index === 2 ? null : work('Current work'),
    next: index === 2 ? [] : [work('Next work')], problem: null,
    steering: { source: index === 0 ? 'session' : 'program', mood: index === 0 ? 'focus' : 'calm',
      session_mood: index === 0 ? 'focus' : null, added: index === 0 ? { include_artists: ['Earth, Wind & Fire'] } : {},
      constraints: index === 0 ? { include_genres: ['classical'], include_artists: ['Earth, Wind & Fire'] } : { energy_bias: -1 },
      expires_at: index === 0 ? 1700000000 : null, expires_in_secs: index === 0 ? 1800 : null },
  }]));
  h.queues = new Map(h.zones().map(zone => [`${zone.coordinator_room}@${zone.household}`, {
    zone: zone.coordinator_room, total: 4, revision: 'queue-' + zone.household, playing: 2, items: [{ position: 1 }],
  }]));
  h.resolution = query => {
    const result = { target: query, matched: query === 'everywhere' ? 'everywhere' : 'alias',
      rooms: query === 'everywhere' ? h.groups.flatMap(group => group.members) : h.selected, groups: h.groups };
    return { ...copy(result), revision: createHash('sha256').update(JSON.stringify([result, h.addressRevision])).digest('hex') };
  };
  h.read = (name, args) => {
    if (name === 'djTargetsResolve') return h.resolution(args.query.target);
    if (name === 'djTargetsRooms') return h.groups.flatMap(group => group.members).map(room => ({ name: room.room,
      household: room.household, zone: room.coordinator, aliases: [target] }));
    if (name === 'djTargetsMoods') return h.moods;
    if (name === 'djTargetsState') return h.states.get(args.params.room);
    if (name === 'djTargetsStatus') return h.statuses.get(args.params.room);
    if (name === 'djTargetsQueue') return h.queues.get(args.params.room);
    if (name === 'djTargetsControl') return { done: h.groups.map(group => `${group.coordinator_room}@${group.household}: DJ action accepted`).join('; '), changed: true, notes: [] };
    throw new Error('Unexpected request: ' + name);
  };
  h.call = async (name, args = {}) => {
    h.calls.push({ name, args });
    const override = h.custom?.(name, args);
    if (override !== undefined) return override;
    if (name === 'djTargetsControl' && h.reply) return h.reply(args.body);
    return copy(h.read(name, args));
  };
  h.http = async (url, body, options) => {
    if (url === '/zones') return copy(h.zones());
    const path = new URL(url, 'http://fixture');
    let name, params = {}, query;
    if (path.pathname === '/dj/control') name = 'djTargetsControl';
    else if (path.pathname === '/rooms/resolve') { name = 'djTargetsResolve'; query = Object.fromEntries(path.searchParams); }
    else if (path.pathname === '/rooms') name = 'djTargetsRooms';
    else if (path.pathname === '/dj/moods') name = 'djTargetsMoods';
    else {
      const parts = path.pathname.split('/'); params.room = decodeURIComponent(parts[2]);
      name = { state: 'djTargetsState', dj: 'djTargetsStatus', queue: 'djTargetsQueue' }[parts[3]];
      if (parts[3] === 'queue') query = Object.fromEntries(path.searchParams);
    }
    if (!name) throw new Error('Unexpected HTTP request ' + url);
    return h.call(name, { ...(body !== undefined ? { body } : {}), ...(query ? { query } : {}),
      ...(params.room ? { params } : {}), ...(options?.signal ? { signal: options.signal } : {}) });
  };
  h.options = { call: h.call, now: () => h.time, changed,
    completed: groups => { h.completed.push(groups); },
    execute: async (groups, work) => { h.executions++; h.held = groups; return h.execute ? h.execute(groups, work) : work(); } };
  if (dashboard) {
    h.controller = C.model({ call: h.http, now: () => h.time, changed: () => h.control?.observe() });
    h.control = T.dashboard({ call: h.http, controller: h.controller, now: () => h.time, changed,
      completed: h.options.completed });
  } else h.control = T.model(h.options);
  h.writes = () => h.calls.filter(call => call.name === 'djTargetsControl');
  h.open = async (action = 'start', replace = false) => {
    if (h.controller) { await h.controller.refresh(); h.controller.connection(true); }
    h.control.open(target); h.control.edit('action', action);
    if (replace || action === 'steer') { h.control.edit('directionMode', 'replace'); h.control.edit('mood', 'focus'); }
  };
  h.review = async (action = 'start', replace = false) => {
    await h.open(action, replace); const pending = await h.control.review();
    assert.ok(pending, h.control.state.message); return pending;
  };
  return h;
}
module.exports = { T, C, fixture, target, work, defer, until };
