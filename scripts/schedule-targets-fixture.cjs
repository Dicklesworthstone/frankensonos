'use strict';
const { createHash } = require('node:crypto');

const target = (name, household) => `${name}@${household}`;
function rooms(groups, aliases = {}) {
  return groups.flatMap(group => group.members.map(name => ({ name, household: group.household,
    zone: group.coordinator_room, aliases: Object.keys(aliases).filter(alias => aliases[alias].includes(target(name, group.household))) })));
}
function resolve(groups, query, aliases = {}, physical = '') {
  const all = groups.flatMap(group => group.members.map(name => ({ room: name, household: group.household,
    player: target(name, group.household), coordinator: target(group.coordinator_room, group.household) })));
  const names = Object.hasOwn(aliases, query) ? aliases[query] : [query];
  const selected = all.filter(room => names.some(name => name === 'all' || name === 'everywhere'
    || name === `all@${room.household}` || name === target(room.room, room.household) || name === room.room));
  if (!selected.length) throw Object.assign(new Error('The saved room is currently absent'), { code: 'UNKNOWN_ROOM' });
  const keys = new Set(selected.map(room => room.coordinator));
  const value = { target: query, matched: Object.hasOwn(aliases, query) ? 'alias'
    : /^(all|everywhere)(@|$)/.test(query) ? 'everywhere' : 'room', rooms: selected,
    groups: groups.filter(group => keys.has(target(group.coordinator_room, group.household))).map(group => ({
      household: group.household, coordinator: target(group.coordinator_room, group.household),
      coordinator_room: group.coordinator_room,
      members: all.filter(room => room.coordinator === target(group.coordinator_room, group.household)),
    })) };
  return { ...value, revision: createHash('sha256').update(JSON.stringify([value, physical])).digest('hex') };
}
module.exports = { rooms, resolve };
