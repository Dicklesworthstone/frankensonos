// Production native queue-copy model: exact occurrences, group lanes, and one
// revision-bound HTTP mutation. No Sonos metadata is accepted from the browser.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const Q = require('../crates/fsonos-api/src/remote/queue-copy.js');
const T = require('../crates/fsonos-api/src/remote/source-targets.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(predicate) {
  for (let attempt = 0; attempt < 500 && !predicate(); attempt++) await Promise.resolve();
  assert.ok(predicate(), 'reached the asynchronous boundary');
}

function fixture(home = 'S1') {
  const zones = ['S1', 'S2'].flatMap(household => [
    { household, coordinator_room: 'Den', members: ['Den', 'Side'] },
    { household, coordinator_room: 'Office', members: ['Office'] },
    { household, coordinator_room: 'Kitchen', members: ['Kitchen'] },
  ]);
  const player = (household, room) => `fixture-${household}-${room}`;
  const tracks = [
    { position: 1, title: 'Original one', uri: 'x-file-cifs://fixture.invalid/Music/one.flac', creator: 'Artist', album: 'First' },
    { position: 2, title: 'Same resource, different occurrence', uri: 'x-file-cifs://fixture.invalid/Music/one.flac', creator: 'Artist', album: 'Second' },
    { position: 3, title: 'Native service selection', uri: 'x-sonos-http:opaque%3Atrack.mp3?sid=fixture&flags=8224&sn=1', creator: 'Another', album: 'Native' },
  ];
  const queues = new Map(zones.map(zone => [`${zone.coordinator_room}@${zone.household}`, {
    zone: zone.coordinator_room, revision: `q1:${zone.household}:${zone.coordinator_room}:7:3`, total: 3, playing: 1,
    items: copy(tracks),
  }]));
  const rows = new Map(zones.map(zone => [`${zone.coordinator_room}@${zone.household}`, {
    zone, transport_state: 'paused', track: { uri: 'fixture:playing', title: 'Current music', queue_position: 1 },
  }]));
  const groupFor = target => zones.find(zone => zone.members.some(room => `${room}@${zone.household}` === target));
  const targets = target => target === 'everywhere' ? zones : target === 'downstairs'
    ? zones.filter(zone => zone.household === home && zone.coordinator_room !== 'Den')
    : target.startsWith('all@') ? zones.filter(zone => zone.household === target.slice(4)) : [groupFor(target)].filter(Boolean);
  const revisions = new Map();
  const room = (zone, name) => ({ household: zone.household, room: name, player: player(zone.household, name),
    coordinator: player(zone.household, zone.coordinator_room) });
  const resolution = target => ({ target, matched: groupFor(target) ? 'room' : 'alias', revision: revisions.get(target) || 'a'.repeat(64),
    rooms: targets(target).map(zone => room(zone, zone.members.find(name => `${name}@${zone.household}` === target) || zone.coordinator_room)),
    groups: targets(target).map(zone => ({ household: zone.household, coordinator_room: zone.coordinator_room,
      coordinator: player(zone.household, zone.coordinator_room), members: zone.members.map(name => room(zone, name)) })),
  });
  const source = `Den@${home}`, sourceQueue = queues.get(source);
  const selection = { zone: source, revision: sourceQueue.revision, total: sourceQueue.total,
    tracks: [copy(tracks[2]), copy(tracks[0]), copy(tracks[2])] };
  const calls = [], lanes = [], receipts = [], laneErrors = [];
  let reader = null, writer = null, lane = null, clock = 100, model;
  const read = (route, options) => {
    if (route === 'zones') return copy(zones);
    if (route === 'sourceTargetResolve') return resolution(options.query.target);
    if (route === 'state') return copy(rows.get(options.params.room));
    if (route === 'sourceQueueState') {
      const zone = groupFor(options.params.room), queue = queues.get(`${zone.coordinator_room}@${zone.household}`);
      return { ...copy(queue), items: copy(queue.items.slice(options.query.offset, options.query.offset + options.query.limit)) };
    }
    throw new Error('Unexpected read ' + route);
  };
  const write = (route, body) => {
    assert.equal(route, 'queueCopy', 'only the native queue-copy endpoint may write');
    const groups = targets(body.target), single = groups.length === 1 ? groups[0] : null;
    const response = { done: 'Selected queue tracks copied', changed: true, notes: [], source_zone: body.zone,
      zone: single ? single.coordinator_room : body.target, positions: copy(body.positions),
      titles: body.positions.map(position => tracks[position - 1].title), mode: body.mode };
    if (body.mode === 'play') return { ...response, ...(single ? {} : { batch: { complete: true, results: groups.map(zone => ({
      household: zone.household, target: player(zone.household, zone.coordinator_room), zone: zone.coordinator_room,
      result: { status: 'succeeded', done: 'Selection playing', changed: true, notes: [] },
    })) } }) };
    return { ...response, queue_batch: { done: 'Selection added to destinations', complete: true, logged: true, undoable: true,
      failed: [], added: groups.map(zone => {
        const queue = queues.get(`${zone.coordinator_room}@${zone.household}`);
        return { household: zone.household, coordinator: player(zone.household, zone.coordinator_room), queue: {
          zone: zone.coordinator_room, done: 'Selected occurrences added', count: body.positions.length,
          first: body.mode === 'next' && queue.playing !== null ? queue.playing + 1 : queue.total + 1, playing_queue: queue.playing !== null,
        } };
      }) } };
  };
  const call = async (route, options = {}) => {
    calls.push({ route, options: { ...options, ...(options.body ? { body: copy(options.body) } : {}) } });
    return options.body ? writer ? writer(route, options.body, write) : write(route, options.body)
      : reader ? reader(route, options, read) : read(route, options);
  };
  const executeSet = async (groups, work) => {
    lanes.push(groups);
    try { const receipt = await (lane ? lane(groups, work) : work()); receipts.push(receipt); return receipt; }
    catch (error) { laneErrors.push(error); throw error; }
  };
  const options = { call, executeSet, now: () => clock };
  model = Q.model(options);
  return { get model() { return model; }, options, selection, source, tracks, zones, queues, rows, calls, lanes, receipts, laneErrors,
    resolution, targets, revisions, read, write, call,
    replaceModel(next) { model = next; }, clock(value) { clock = value; },
    reader(fn) { reader = fn; }, writer(fn) { writer = fn; }, lane(fn) { lane = fn; },
    writes: () => calls.filter(call => call.options.body),
    async opened(value = selection) { assert.equal(await model.open(value), true, model.state.message); },
    async review(target = `Office@${home}`, mode = 'play') {
      model.edit('target', target); model.edit('mode', mode);
      assert.equal(await model.review(), true, model.state.message); return model.state.pending;
    },
  };
}

for (const home of ['S1', 'S2']) for (const mode of ['play', 'next', 'end']) for (const targetKind of ['room', 'alias', 'all']) {
  test(`${home} ${mode} to ${targetKind} copies exact ordered native occurrences with one request`, async () => {
    const f = fixture(home); await f.opened();
    const target = targetKind === 'room' ? `Office@${home}` : targetKind === 'alias' ? 'downstairs' : `all@${home}`;
    const pending = await f.review(target, mode);
    assert.equal(pending.targets.item.kind, 'native_queue_tracks');
    assert.equal(f.writes().length, 0);
    assert.equal(T.reviewGroups(pending.targets).length, f.targets(target).length);
    assert.equal(await f.model.confirm(pending), true, f.model.state.result?.detail);
    assert.equal(await f.model.confirm(pending), false);
    assert.equal(f.writes().length, 1);
    const body = f.writes()[0].options.body;
    assert.deepEqual(body, { zone: f.source, positions: [3, 1, 3], expected_revision: f.selection.revision,
      mode: mode === 'end' ? 'append' : mode, target, expected_target_revision: 'a'.repeat(64),
      ...(targetKind === 'room' ? { expected_queue_revision: f.queues.get(target).revision } : {}) });
    assert.equal(f.model.state.result.complete, true);
    assert.equal(f.receipts[0], f.model.state.result, 'the shared execution lane receives the validated receipt');
    assert.equal(f.receipts[0].changed, true);
    const sourceLane = f.lanes[0].filter(group => group.coordinator === 'Den');
    assert.equal(sourceLane.length, 1);
    assert.equal(sourceLane[0].sourceOnly === true, targetKind !== 'all');
    assert.ok(f.lanes[0].every(group => group.shape === group.signature && group.members.every(room => room.household && room.room && room.player)));
    assert.equal(pending.targets.groups.some(group => group.sourceOnly), false);
  });
}

test('the original qualified member room remains the source while its full group is reserved', async () => {
  const f = fixture(); await f.opened({ ...f.selection, zone: 'Side@S1' });
  const pending = await f.review(); assert.equal(await f.model.confirm(pending), true);
  assert.equal(f.writes()[0].options.body.zone, 'Side@S1');
  assert.deepEqual(f.lanes[0].find(group => group.sourceOnly).members.map(room => room.room), ['Den', 'Side']);
});

test('selection admission excludes raw metadata, conflicting repeats, absent resources and oversized native values', () => {
  const { selection } = fixture();
  const bad = [null, { ...selection, zone: 'Den' }, { ...selection, metadata: '<DIDL-Lite/>' },
    { ...selection, revision: 'a'.repeat(1025) }, { ...selection, revision: '🐈'.repeat(257) },
    { ...selection, tracks: [] }, { ...selection, tracks: Array(11).fill(selection.tracks[0]) },
    ...[{ metadata: '<DIDL-Lite/>' }, { position: 0 }, { position: 100001 }, { uri: null }, { uri: 'NOT_IMPLEMENTED' },
      { uri: 'x-file:' + '🐈'.repeat(16384) }, { title: 'bad\u0000title' }].map(fields => ({ ...selection, tracks: [{ ...selection.tracks[0], ...fields }] })),
    { ...selection, tracks: [selection.tracks[0], { ...selection.tracks[0], title: 'A different occurrence' }] },
  ];
  for (const value of bad) assert.throws(() => Q.selectionOf(value));
  assert.deepEqual(Q.selectionOf(selection), selection);
  assert.ok(Object.isFrozen(Q.selectionOf(selection).tracks[0]));
});

test('reordering, repeating and removing keep exact original positions without editing the source queue', async () => {
  const f = fixture(); await f.opened();
  assert.equal(f.model.changeSelection('up', 1), true);
  assert.equal(f.model.changeSelection('down', 1), true);
  assert.equal(f.model.changeSelection('repeat', 0), true);
  assert.deepEqual(f.model.state.selection.tracks.map(track => track.position), [1, 1, 3, 3]);
  const pending = await f.review();
  assert.equal(f.model.changeSelection('remove', 1), true);
  assert.equal(await f.model.confirm(pending), false);
  while (f.model.state.selection.tracks.length) f.model.changeSelection('remove', 0);
  assert.equal(await f.model.review(), false); assert.equal(f.writes().length, 0);
  assert.deepEqual(f.queues.get(f.source).items, f.tracks);
});

test('a room set that spans households is rejected before a copy request', async () => {
  const f = fixture(); await f.opened(); f.model.edit('target', 'everywhere');
  assert.equal(await f.model.review(), false); assert.match(f.model.state.message, /original household/);
  assert.equal(f.writes().length, 0);
});

for (const mutation of ['revision', 'total', 'title', 'uri', 'creator', 'album', 'physical', 'membership']) {
  test(`changed source ${mutation} consumes confirmation without writing`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review();
    if (mutation === 'revision') f.queues.get(f.source).revision += ':changed';
    else if (mutation === 'total') f.queues.get(f.source).total++;
    else if (mutation === 'physical') f.revisions.set(f.source, 'b'.repeat(64));
    else if (mutation === 'membership') f.zones.find(zone => zone.household === 'S1' && zone.coordinator_room === 'Den').members.pop();
    else f.queues.get(f.source).items[2][mutation] += ':changed';
    assert.equal(await f.model.confirm(pending), false);
    assert.equal(f.model.state.result.status, 'not_sent');
    assert.equal(f.writes().length, 0); assert.equal(f.model.state.pending, null);
  });
}

test('same display names with a changed source player identity cannot authorize copying', async () => {
  const f = fixture(); await f.opened(); const pending = await f.review();
  f.reader((route, args, read) => {
    const value = read(route, args);
    if (route === 'sourceTargetResolve' && args.query.target === f.source) {
      const rename = room => { if (room.player.endsWith('-Side')) room.player += '-replacement'; };
      value.rooms.forEach(rename); value.groups[0].members.forEach(rename);
    }
    return value;
  });
  assert.equal(await f.model.confirm(pending), false); assert.equal(f.writes().length, 0);
});

for (const mutation of ['revision', 'queue', 'playback']) {
  test(`destination ${mutation} changes require a new review`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review('downstairs', 'next');
    if (mutation === 'revision') f.revisions.set('downstairs', 'b'.repeat(64));
    else if (mutation === 'queue') f.queues.get('Office@S1').revision += ':changed';
    else f.rows.get('Office@S1').track.queue_position = 2;
    assert.equal(await f.model.confirm(pending), false); assert.equal(f.writes().length, 0);
  });
}

test('append permits ordinary destination playhead progress; next binds its exact current occurrence', async () => {
  for (const mode of ['next', 'end']) {
    const f = fixture(); await f.opened(); const pending = await f.review('Office@S1', mode);
    f.rows.get('Office@S1').track.queue_position = 2; f.queues.get('Office@S1').playing = 2;
    assert.equal(await f.model.confirm(pending), mode === 'end');
    assert.equal(f.writes().length, mode === 'end' ? 1 : 0);
  }
});

test('review lifetime begins after all source and destination reads finish', async () => {
  const f = fixture(); await f.opened(); let clock = 100;
  f.reader((route, options, read) => { f.clock(clock += 5000); return read(route, options); });
  const pending = await f.review(); assert.equal(pending.reviewedAt, clock);
  f.reader(null); assert.equal(await f.model.confirm(pending), true);
  const second = fixture(); await second.opened(); const expired = await second.review();
  second.clock(expired.reviewedAt + Q.REVIEW_AGE_MS + 1);
  assert.equal(await second.model.confirm(expired), false); assert.equal(second.model.state.pending, null);
  assert.equal(second.writes().length, 0); assert.match(second.model.state.message, /expired/);
});

for (const action of ['close', 'suspend', 'dispose']) {
  test(`${action} during confirmation reads prevents all late writes`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review(), hold = deferred(); let waiting = false;
    f.reader(async (route, options, read) => { waiting = true; await hold.promise; return read(route, options); });
    const confirming = f.model.confirm(pending); await until(() => waiting); f.model[action](); hold.resolve();
    assert.equal(await confirming, false); assert.equal(f.writes().length, 0);
    f.model.resume(); assert.equal(await f.model.confirm(pending), false);
  });
  test(`${action} after dispatch retains its acknowledgement and blocks a second command`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review(), hold = deferred();
    f.writer(async (route, body, write) => { await hold.promise; return write(route, body); });
    const confirming = f.model.confirm(pending); await until(() => f.writes().length === 1); f.model[action]();
    assert.equal(await f.model.open(f.selection), false); hold.resolve();
    assert.equal(await confirming, true); assert.equal(f.model.state.result.complete, true);
    assert.equal(f.writes().length, 1);
  });
}

test('closing while a reservation waits cannot send later; a lane cannot invoke the work twice', async () => {
  const f = fixture(); await f.opened(); const pending = await f.review(), hold = deferred(); let waiting = false;
  f.lane(async (_, work) => { waiting = true; await hold.promise; return work(); });
  const confirming = f.model.confirm(pending); await until(() => waiting); f.model.close(); hold.resolve();
  assert.equal(await confirming, false); assert.equal(f.writes().length, 0);
  const other = fixture(); await other.opened(); const second = await other.review();
  other.lane(async (_, work) => { const result = await work(); await assert.rejects(work, /already been consumed/); return result; });
  assert.equal(await other.model.confirm(second), true); assert.equal(other.writes().length, 1);
});

test('superseding an in-flight open cannot resurrect an old source selection', async () => {
  const f = fixture(), hold = deferred(); let waiting = false;
  f.reader(async (route, options, read) => { waiting = true; await hold.promise; return read(route, options); });
  const opening = f.model.open(f.selection); await until(() => waiting); f.model.close(); hold.resolve();
  assert.equal(await opening, false); assert.equal(f.model.state.open, false); assert.equal(f.model.state.sourceGroup, null);
  assert.equal(f.writes().length, 0);
});

test('a lost acknowledgement propagates to the shared lane and is never retried', async () => {
  const f = fixture(); await f.opened(); const pending = await f.review('downstairs', 'next');
  const failure = new Error('Connection closed after request'); f.writer(() => { throw failure; });
  assert.equal(await f.model.confirm(pending), false); assert.equal(f.model.state.result.uncertain, true);
  assert.equal(f.laneErrors[0], failure); assert.equal(await f.model.confirm(pending), false);
  assert.equal(f.writes().length, 1); assert.match(f.model.state.result.detail, /not repeated/);
});

for (const wrong of ['source_zone', 'zone', 'mode', 'positions', 'titles', 'queue_batch', 'count', 'missing_group', 'duplicate_group']) {
  test(`a mismatched ${wrong} receipt remains uncertain without repeating the copy`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review('downstairs', 'end');
    f.writer((route, body, write) => {
      const value = write(route, body);
      if (wrong === 'positions') value.positions = [1, 3, 3];
      else if (wrong === 'titles') value.titles[0] = 'Another title';
      else if (wrong === 'queue_batch') delete value.queue_batch;
      else if (wrong === 'count') value.queue_batch.added[0].queue.count = 1;
      else if (wrong === 'missing_group') value.queue_batch.added.pop();
      else if (wrong === 'duplicate_group') value.queue_batch.added[1] = copy(value.queue_batch.added[0]);
      else value[wrong] = 'wrong';
      return value;
    });
    assert.equal(await f.model.confirm(pending), false); assert.equal(f.model.state.result.uncertain, true);
    assert.equal(f.laneErrors.length, 1); assert.equal(f.writes().length, 1);
  });
}

test('multi-group native insertions retain actual later offsets; single-group offsets remain revision-bound', async () => {
  for (const target of ['downstairs', 'Office@S1']) {
    const f = fixture(); await f.opened(); const pending = await f.review(target, 'end');
    f.writer((route, body, write) => { const value = write(route, body); value.queue_batch.added[0].queue.first = 41; return value; });
    assert.equal(await f.model.confirm(pending), target === 'downstairs');
    assert.equal(f.writes().length, 1);
    if (target === 'downstairs') assert.match(f.model.state.result.detail, /position 41/);
    else assert.equal(f.model.state.result.uncertain, true);
  }
});

for (const allFailed of [false, true]) {
  test(`${allFailed ? 'all-failed' : 'partial'} queue receipts retain every destination and actual changed state`, async () => {
    const f = fixture(); await f.opened(); const pending = await f.review('downstairs', 'end');
    f.writer((route, body, write) => {
      const value = write(route, body), batch = value.queue_batch;
      const failed = batch.added.splice(0, allFailed ? 2 : 1);
      batch.failed = failed.map(row => ({ household: row.household, coordinator: row.coordinator, zone: row.queue.zone,
        error: { code: 'OUTCOME_UNCERTAIN', detail: 'The speaker did not confirm insertion', retryable: false } }));
      batch.complete = false; batch.undoable = false; value.changed = !allFailed; return value;
    });
    assert.equal(await f.model.confirm(pending), true); assert.equal(f.receipts[0].changed, !allFailed);
    assert.equal(f.model.state.result.partial, true); assert.equal(f.model.state.result.uncertain, true);
    assert.match(f.model.state.result.detail, /Office@S1/); assert.match(f.model.state.result.detail, /Kitchen@S1/);
    assert.match(f.model.state.result.detail, /Do not repeat the whole batch/); assert.equal(f.writes().length, 1);
  });
}

test('partial playback and all-failed playback error receipts preserve target-specific outcomes', async () => {
  for (const errorResponse of [false, true]) {
    const f = fixture(); await f.opened(); const pending = await f.review('downstairs');
    f.writer((route, body, write) => {
      const value = write(route, body); value.batch.complete = false;
      value.batch.results[0].result = { status: 'failed', error: { code: 'OUTCOME_UNCERTAIN', detail: 'Play acknowledgement lost', retryable: false } };
      if (errorResponse) {
        value.batch.results[1].result = { status: 'not_attempted' };
        throw Object.assign(new Error('Playback could not complete'), { batch: value.batch });
      }
      return value;
    });
    assert.equal(await f.model.confirm(pending), !errorResponse); assert.equal(f.model.state.result.partial, true);
    assert.equal(f.model.state.result.uncertain, true); assert.match(f.model.state.result.detail, /Office@S1/);
    assert.match(f.model.state.result.detail, /Kitchen@S1/); assert.equal(f.writes().length, 1);
  }
});

test('real HTTP route adapter preserves original source, ordered positions and destination preconditions', async t => {
  const f = fixture('S2'), requests = [];
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://127.0.0.1'), query = Object.fromEntries(url.searchParams);
      let body; if (req.method === 'POST') { let raw = ''; for await (const chunk of req) raw += chunk; body = JSON.parse(raw); }
      requests.push({ method: req.method, path: url.pathname, query, body });
      let value;
      if (url.pathname === '/zones') value = f.read('zones', {});
      else if (url.pathname === '/rooms/resolve') value = f.read('sourceTargetResolve', { query });
      else if (url.pathname === '/queue/selection') value = f.write('queueCopy', body);
      else {
        const [, , encoded, kind] = url.pathname.split('/'), params = { room: decodeURIComponent(encoded) };
        value = f.read(kind === 'state' ? 'state' : 'sourceQueueState', { params,
          query: { offset: Number(query.offset), limit: Number(query.limit) } });
      }
      res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(value));
    } catch (error) { res.writeHead(500); res.end(JSON.stringify({ error: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const model = Q.model({ ...f.options, async call(route, options = {}) {
    const [method, path] = Q.routes[route];
    const url = origin + path.replace('{room}', encodeURIComponent(options.params?.room)) + (options.query ? '?' + new URLSearchParams(options.query) : '');
    const response = await fetch(url, { method, signal: options.signal, ...(options.body ? {
      headers: { 'content-type': 'application/json' }, body: JSON.stringify(options.body),
    } : {}) });
    assert.equal(response.status, 200); return response.json();
  } });
  f.replaceModel(model); await f.opened(); const pending = await f.review('downstairs', 'next');
  assert.equal(await model.confirm(pending), true, model.state.result?.detail);
  const mutations = requests.filter(request => request.method === 'POST'); assert.equal(mutations.length, 1);
  assert.equal(mutations[0].path, '/queue/selection'); assert.equal(mutations[0].body.zone, 'Den@S2');
  assert.deepEqual(mutations[0].body.positions, [3, 1, 3]); assert.equal(mutations[0].body.expected_target_revision, 'a'.repeat(64));
  assert.equal(Object.hasOwn(mutations[0].body, 'expected_queue_revision'), false);
  assert.ok(requests.some(request => request.path === '/zones/Den%40S2/queue' && request.query.offset === '2'));
});
