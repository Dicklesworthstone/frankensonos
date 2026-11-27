// Noncontiguous native selections use bounded, revision-bound observations of
// the original playlist; every surface sends only the saved occurrence IDs.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const T = require('../crates/fsonos-api/src/remote/source-targets.js');
const copy = value => structuredClone(value);

function fixture(positions = [310, 1, 151, 1, 330]) {
  const original = { household: 'S2', coordinator_room: 'Den', members: ['Den'] };
  const group = P.groups([original])[0];
  const playlist = { id: 'SQ:7', title: 'Long saved playlist' }, revision = 'saved-original-revision';
  const total = 100000;
  const row = position => ({ position, title: 'Saved occurrence ' + position,
    uri: position === 1 || position === 151 ? 'fixture:repeat' : 'fixture:track-' + position });
  const page = { zone: 'Den', playlist, revision, total, offset: 0,
    items: [...new Set(positions)].map(row), next_offset: null };
  const choice = P.preparePlaylistTracks(page, positions, 'play');
  const f = { original, group, playlist, revision, total, row, page, choice, calls: [], change: null };
  f.call = async (name, args) => {
    assert.equal(name, 'sourcePlaylistContents', 'selection verification performs only native catalog reads');
    f.calls.push(copy(args.query));
    const q = args.query;
    assert.equal(q.zone, group.target);
    assert.equal(q.playlist, playlist.id);
    assert.equal(q.expected_revision, revision);
    assert.ok(q.limit > 0 && q.limit <= 100, 'each request obeys the native page bound');
    const value = { zone: 'Den', playlist: copy(playlist), revision, total, offset: q.offset,
      items: Array.from({ length: Math.min(q.limit, total - q.offset) }, (_, i) => row(q.offset + i + 1)),
      next_offset: q.offset + q.limit < total ? q.offset + q.limit : null };
    f.change?.(value, q, f.calls.length);
    return value;
  };
  f.read = more => P.readPlaylistTracks({ call: f.call, item: choice.item, group, ...more });
  return f;
}

test('scattered selected occurrences are grouped into small original-source reads without losing order or repeats', async () => {
  const f = fixture();
  await f.read();
  assert.deepEqual(f.calls.map(q => [q.offset, q.limit]), [[0, 1], [150, 1], [309, 21]]);
  assert.deepEqual(f.choice.item.positions, [310, 1, 151, 1, 330]);
  assert.deepEqual(f.choice.item.tracks.map(track => track.position), [310, 1, 151, 1, 330]);
  assert.equal(f.calls.reduce((n, query) => n + query.limit, 0), 23, 'no full-playlist read');
});

test('the 100-row boundary and the final supported occurrence remain selectable', async () => {
  const f = fixture([100000, 1, 100, 101, 99999]);
  await f.read();
  assert.deepEqual(f.calls.map(q => [q.offset, q.limit]), [[0, 100], [100, 1], [99998, 2]]);
});

test('repeated selections share verification reads but remain repeated in the native command', async () => {
  const f = fixture([151, 151, 151]);
  await f.read();
  assert.equal(f.calls.length, 1);
  assert.deepEqual(f.calls.map(q => [q.offset, q.limit]), [[150, 1]]);
  assert.deepEqual(P.commandBody(f.choice, f.group.target).positions, [151, 151, 151]);
});

test('ten distant occurrences use at most ten bounded reads', async () => {
  const positions = Array.from({ length: 10 }, (_, i) => 1 + i * 1000).reverse();
  const f = fixture(positions);
  await f.read();
  assert.equal(f.calls.length, 10);
  assert.ok(f.calls.every(query => query.limit === 1));
  assert.deepEqual(f.choice.item.positions, positions);
});

for (const [name, change] of [
  ['revision', value => { value.revision = 'newer-revision'; }],
  ['size', value => { value.total++; }],
  ['playlist', value => { value.playlist.id = 'SQ:9'; }],
  ['title', value => { value.playlist.title = 'Renamed'; }],
  ['occurrence', value => { value.items[0].position++; }],
  ['resource', value => { value.items[0].uri = 'fixture:other'; }],
  ['track title', value => { value.items[0].title = 'Different track'; }],
]) {
  test('a changed ' + name + ' on a later selected page refuses the entire selection', async () => {
    const f = fixture();
    f.change = (value, _query, number) => { if (number === 2) change(value); };
    await assert.rejects(f.read());
    assert.equal(f.calls.length, 2, 'later pages are not adopted after failed verification');
  });
}

test('failed catalog reads do not publish an incomplete selection as ready', async () => {
  const f = fixture();
  const original = f.call;
  f.call = async (...args) => {
    if (f.calls.length === 1) throw new Error('catalog unavailable');
    return original(...args);
  };
  await assert.rejects(f.read(), /catalog unavailable/);
  assert.equal(f.calls.length, 1);
});

test('validity is checked before the first read and after every returned page', async () => {
  const f = fixture();
  await assert.rejects(f.read({ valid() { throw new Error('closed'); } }), /closed/);
  assert.equal(f.calls.length, 0);
  let current = true;
  f.change = () => { current = false; };
  await assert.rejects(f.read({ valid() { if (!current) throw new Error('changed draft'); } }), /changed draft/);
  assert.equal(f.calls.length, 1);
});

for (const mode of ['play', 'next', 'end']) {
  test('room-set ' + mode + ' reviews every selected page at the original reader and sends one native payload', async () => {
    const f = fixture(), target = 'downstairs';
    const rooms = ['Kitchen', 'Office'].map(room => ({ household: 'S2', room,
      player: 'fixture-' + room, coordinator: 'fixture-' + room }));
    const resolution = { target, matched: 'alias', revision: 'a'.repeat(64), rooms,
      groups: rooms.map(room => ({ household: room.household, coordinator: room.coordinator,
        coordinator_room: room.room, members: [room] })) };
    const zones = [f.original, ...rooms.map(room => ({ household: room.household,
      coordinator_room: room.room, members: [room.room] }))];
    const call = async (name, args) => {
      if (name === 'sourcePlaylistContents') return f.call(name, args);
      if (name === 'zones') return copy(zones);
      if (name === 'sourceTargetResolve') return copy(resolution);
      const zone = zones.find(zone => zone.coordinator_room + '@S2' === args.params?.room);
      if (name === 'state') return { zone, transport_state: 'paused',
        track: { uri: 'fixture:current', title: 'Current music', queue_position: 1 } };
      if (name === 'sourceQueueState') return { zone: zone.coordinator_room, total: 2,
        revision: 'queue-' + zone.coordinator_room, playing: 1, items: [{ position: 1 }] };
      throw new Error('Unexpected route ' + name);
    };
    const choice = P.preparePlaylistTracks(f.page, f.choice.item.positions, mode);
    const reviewed = await T.inspect({ call, player: P, target, item: choice.item, mode, sourceGroup: f.group });
    assert.equal(reviewed.groups.length, 2);
    assert.equal(f.calls.length, 3);
    const second = await T.inspect({ call, player: P, target, item: choice.item, mode, sourceGroup: f.group });
    assert.equal(second.signature, reviewed.signature);
    assert.equal(f.calls.length, 6, 'confirmation verifies every retained page again');
    const request = T.request(choice, reviewed, P);
    assert.equal(request.route, 'sourcePlaylistTracks');
    assert.deepEqual(request.body, { zone: 'Den@S2', playlist: 'SQ:7', positions: [310, 1, 151, 1, 330],
      expected_revision: f.revision, mode: mode === 'end' ? 'append' : mode,
      target, expected_target_revision: 'a'.repeat(64) });
  });
}
