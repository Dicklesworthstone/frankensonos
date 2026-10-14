'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const Groups = require('../crates/fsonos-api/src/remote/groups.js');
const group = (household, room, ...members) => ({ household, coordinator_room: room,
  members: [room, ...members], degraded: [] });
const topology = () => [group('S1', 'Den', 'Study'), group('S1', 'Kitchen'), group('S2', 'Den', 'Hall')];
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};
function fixture() {
  let groups = topology();
  let read = (room) => {
    const zone = groups.find((g) => g.members.some((m) => `${m}@${g.household}` === room));
    return { zone, transport_state: 'playing', track: {
      uri: `spotify:track:${zone.coordinator_room}`, title: zone.coordinator_room,
      queue_position: 2, position_secs: 42,
    } };
  };
  let response = () => ({ done: 'music moved', changed: true, notes: [] });
  const calls = [];
  const model = Groups.model({ call: async (route, args) => {
    calls.push([route, args]);
    if (route === 'zones') return structuredClone(groups);
    if (route === 'state') return read(args.params.room);
    return response();
  } });
  return { model, calls, read(fn) { const before = read; read = (room) => fn(room, before); },
    response(fn) { response = fn; }, groups(value) { groups = value; },
    writes: () => calls.filter(([route]) => route.startsWith('groups')) };
}
async function ready(f, source = 'Study@S1', destination = 'Kitchen@S1') {
  await f.model.refresh();
  f.model.choose('source', source); f.model.choose('destination', destination);
}

test('native move reads and reviews both groups then sends the exact existing request once', async () => {
  const f = fixture(); await ready(f);
  assert.equal(await f.model.reviewTransfer('move'), true);
  assert.equal(f.model.state.pending.audio.length, 2);
  assert.equal(f.model.state.pending.audio[0].label, 'Den');
  assert.equal(await f.model.confirm(), true);
  assert.deepEqual(f.writes(), [['groupsMove', { body: { zone:'Study@S1', to:'Kitchen@S1', copy:false } }]]);
  assert.equal(f.calls.filter(([route]) => route === 'state').length, 4);
});

test('a native handoff within an existing group is allowed, with one affected group', async () => {
  const f = fixture(); await ready(f, 'Den@S1', 'Study@S1');
  assert.equal(await f.model.reviewTransfer('move'), true);
  assert.equal(f.model.state.pending.labels.length, 1);
  assert.equal(await f.model.confirm(), true);
  assert.equal(f.writes()[0][1].body.copy, false);
});

test('cross-household replay explicitly stops the source and targets the destination group', async () => {
  const f = fixture(); await ready(f, 'Study@S1', 'Hall@S2');
  assert.equal(await f.model.reviewTransfer('replay'), true);
  assert.match(f.model.state.pending.warning, /stops the source group/);
  assert.match(f.model.state.pending.warning, /does not keep both playing/);
  assert.match(f.model.state.pending.title, /Den \+ Hall \(S2\)/);
  assert.equal(await f.model.confirm(), true);
  assert.deepEqual(f.writes()[0][1].body, { zone:'Study@S1', to:'Hall@S2', copy:true });
});

test('cross-household native moves and same-group replay never read playback or write', async () => {
  for (const [to, kind] of [['Hall@S2','move'], ['Den@S1','replay'], ['Study@S1','move']]) {
    const f = fixture(); await ready(f, 'Study@S1', to);
    assert.equal(await f.model.reviewTransfer(kind), false);
    assert.equal(await f.model.confirm(), false);
    assert.equal(f.calls.some(([route]) => route === 'state'), false);
    assert.deepEqual(f.writes(), []);
  }
});

test('the ordinary grouping review cannot bypass transfer playback reads', async () => {
  const f = fixture(); await ready(f);
  assert.equal(f.model.review('move'), false);
  assert.equal(f.model.review('replay'), false);
  assert.equal(await f.model.confirm(), false);
  assert.deepEqual(f.writes(), []);
});

test('empty sources and incomplete or transitioning playback cannot be moved', async () => {
  for (const patch of [{track:undefined}, {track:null}, {transport_state:'unknown'},
    {transport_state:'transitioning'}, {track:{uri:'NOT_IMPLEMENTED'}},
    {track:{uri:'spotify:track:x',queue_position:0}}, {zone:group('S2','Other')}]) {
    const f = fixture(); await ready(f);
    f.read((room, before) => ({...before(room),...patch}));
    assert.equal(await f.model.reviewTransfer('move'), false);
    assert.equal(await f.model.confirm(), false);
    assert.deepEqual(f.writes(), []);
  }
});

test('a paused source and an idle destination are valid handoff observations', async () => {
  const f = fixture(); await ready(f);
  f.read((room, before) => room === 'Study@S1'
    ? {...before(room),transport_state:'paused'}
    : {...before(room),transport_state:'stopped',track:null});
  assert.equal(await f.model.reviewTransfer('move'), true);
  assert.equal(f.model.state.pending.audio[1].label, 'Nothing loaded');
  assert.equal(await f.model.confirm(), true);
});

test('a source or destination track switch after review prevents any mutation', async () => {
  for (const changed of ['Study@S1','Kitchen@S1']) {
    const f = fixture(); await ready(f); await f.model.reviewTransfer('move');
    f.read((room,before) => room === changed ? {...before(room),track:{uri:'spotify:track:new'}} : before(room));
    assert.equal(await f.model.confirm(), false);
    assert.match(f.model.state.message,/playback changed/);
    assert.deepEqual(f.writes(), []);
  }
});

test('queue position and pause changes invalidate a review even for an identical URI', async () => {
  for (const patch of [{transport_state:'paused'}, {track:{uri:'spotify:track:Den',queue_position:3}}]) {
    const f = fixture(); await ready(f); await f.model.reviewTransfer('move');
    f.read((room,before) => room === 'Study@S1' ? {...before(room),...patch} : before(room));
    assert.equal(await f.model.confirm(), false);
    assert.deepEqual(f.writes(), []);
  }
});

test('natural elapsed time and cosmetic title changes do not restart the review', async () => {
  const f = fixture(); await ready(f); await f.model.reviewTransfer('move');
  f.read((room,before) => {
    const state=before(room);return {...state,track:{...state.track,position_secs:99,title:'New display title'}};
  });
  assert.equal(await f.model.confirm(), true);
  assert.equal(f.writes().length,1);
});

test('late transfer review cannot overwrite a refreshed or closed panel', async () => {
  for (const action of ['refresh','close']) {
    const f = fixture(); await ready(f); const gate=deferred();
    f.read(async (room,before) => { await gate.promise;return before(room); });
    const review=f.model.reviewTransfer('move'); await f.model[action]();
    gate.resolve(); assert.equal(await review,false);
    assert.equal(f.model.state.pending,null); assert.deepEqual(f.writes(),[]);
  }
});

test('closing during final playback reads prevents transfer dispatch', async () => {
  const f=fixture();await ready(f);await f.model.reviewTransfer('replay');
  const gate=deferred();f.read(async (room,before)=>{await gate.promise;return before(room);});
  const write=f.model.confirm();await Promise.resolve();f.model.close();
  gate.resolve();assert.equal(await write,false);assert.deepEqual(f.writes(),[]);
});

test('a failed final playback read sends no mutation', async () => {
  const f=fixture();await ready(f);await f.model.reviewTransfer('move');
  f.read(()=>{throw new Error('source disappeared');});
  assert.equal(await f.model.confirm(),false);assert.deepEqual(f.writes(),[]);
});

test('an uncertain handoff is never replayed or replaced by a copy attempt', async () => {
  for (const response of [()=>null,()=>{throw new Error('lost response');}]) {
    const f=fixture();await ready(f);await f.model.reviewTransfer('move');f.response(response);
    assert.equal(await f.model.confirm(),false);
    assert.equal(await f.model.confirm(),false);
    assert.equal(await f.model.reviewTransfer('replay'),false);
    assert.equal(f.writes().length,1);
    assert.equal(f.writes()[0][1].body.copy,false);
  }
});

test('a group change during the final playback reads cancels the transfer', async () => {
  const f=fixture();await ready(f);await f.model.reviewTransfer('move');
  f.read((room,before)=>{ const state=before(room); f.groups([...topology().slice(0,1),group('S1','Kitchen','Guest')]);return state; });
  assert.equal(await f.model.confirm(),false);
  assert.deepEqual(f.writes(),[]);
});
