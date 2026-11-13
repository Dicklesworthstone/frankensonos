'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Source = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const copy = value => structuredClone(value);
const URI = 'spotify:track:0123456789ABCDEFGHIJKL';
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
async function until(check) {
  for (let n = 0; n < 300 && !check(); n++) await Promise.resolve();
  assert.ok(check(), 'expected asynchronous boundary');
}
async function fixture() {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Den',
    members: household === 'S2' ? ['Den', 'Side'] : ['Den'], transport_state: 'paused' }));
  const rows = new Map(zones.map(zone => [C.key(zone), { zone, transport_state: 'paused', volume: 12, mute: false,
    track: { uri: 'fixture:current', title: 'Current music', queue_position: 2, position_secs: 20, duration_secs: 300 } }]));
  const queues = new Map(zones.map((zone, i) => [C.key(zone), { zone: 'Den', total: 5 + i, playing: i ? null : 2,
    revision: 'queue-' + zone.household, items: [{ position: 1, title: 'Retained song', uri: 'fixture:first' }] }]));
  const catalogs = new Map(zones.map((zone, i) => [C.key(zone), [{ id: 'SQ:' + (i + 7), title: 'Evening' }]]));
  const favorites = new Map(zones.map((zone, i) => [C.key(zone), [{ id: 'FV:2/' + (i + 4), title: 'Morning', kind: 'track' }]]));
  const calls = [], completed = [];
  let reader = null, writer = null, source;
  const player = (home, room) => 'fixture-' + home + '-' + room;
  const resolution = target => ({ target, matched: 'alias',
    rooms: zones.map(z => ({ household: z.household, room: 'Den', player: player(z.household, 'Den'), coordinator: player(z.household, 'Den') })),
    groups: zones.map(z => ({ household: z.household, coordinator: player(z.household, 'Den'), coordinator_room: 'Den',
      members: z.members.map(room => ({ household: z.household, room, player: player(z.household, room), coordinator: player(z.household, 'Den') })) })) });
  const batch = failed => ({ done: failed ? 'One group could not add the music' : 'Added to both groups', complete: !failed,
    added: zones.filter((_, i) => !failed || !i).map(z => ({ household: z.household, coordinator: player(z.household, 'Den'),
      queue: { zone: 'Den', done: 'Music queued', first: z.household === 'S2' ? 3 : 7, count: 1, playing_queue: z.household === 'S2' } })),
    failed: failed ? [{ household: 'S1', coordinator: player('S1', 'Den'), zone: 'Den',
      error: { code: 'UPNP_FAULT', detail: 'The S1 speaker refused the addition', retryable: false, hint: 'Read this queue before another add.' } }] : [],
    logged: true, undoable: true });
  const read = url => {
    if (url === '/zones') return copy(zones);
    if (url.startsWith('/rooms/resolve?')) return resolution(new URLSearchParams(url.split('?')[1]).get('target'));
    if (url === '/rooms') return zones.map(z => ({ name: 'Den', household: z.household, zone: 'Den', aliases: ['downstairs'] }));
    if (url.startsWith('/sonos/playlists?')) return copy(catalogs.get(new URLSearchParams(url.split('?')[1]).get('zone')));
    if (url.startsWith('/favorites?')) return copy(favorites.get(new URLSearchParams(url.split('?')[1]).get('zone')));
    const target = decodeURIComponent(url.split('/')[2] || '');
    if (url.endsWith('/state')) return copy(rows.get(target));
    if (url.endsWith('/queue?offset=0&limit=1')) return copy(queues.get(target));
    throw new Error('Unexpected read ' + url);
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body) : url === '/queue' ? batch(false) : { done: 'Started both groups', changed: true, notes: [] };
    const override = reader?.(url, options);
    return override === undefined ? read(url) : override;
  };
  const controller = C.model({ call, changed: () => source?.observe() });
  source = Source.model({ call, controller, completed: result => { completed.push(result); } });
  await controller.refresh(); controller.connection(true);
  return { source, controller, calls, completed, zones, rows, queues, catalogs, favorites, batch, resolution,
    write(fn) { writer = fn; }, read(fn) { reader = fn; }, writes: () => calls.filter(c => c.body !== undefined),
    async review(mode = 'play', kind = 'link') {
      source.open(); source.edit('destinationMode', 'set'); source.edit('destinationInput', 'downstairs');
      source.edit('sourceType', kind); source.edit('mode', mode);
      source.edit(kind === 'link' ? 'input' : 'nativeQuery', kind === 'link' ? URI : kind === 'favorite' ? 'Morning' : 'Evening');
      const reviewed = await source.review(); assert.ok(reviewed, source.state.message); return reviewed;
    } };
}

for (const mode of ['play', 'next', 'end']) test(`dashboard ${mode} reviews all groups and sends one collective source command`, async () => {
  const f = await fixture(), review = await f.review(mode);
  assert.equal(review.targets.groups.length, 2);
  assert.ok(review.targets.entries.some(e => e.group.label.includes('Side')));
  assert.equal(f.writes().length, 0);
  assert.equal(await f.source.apply(review), true);
  assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: mode === 'play' ? '/play' : '/queue',
    body: { zone: 'downstairs', source_uri: URI, ...(mode === 'play' ? {} : { next: mode === 'next' }) } }]);
  assert.equal(await f.source.apply(review), false);
  assert.deepEqual(f.completed[0].targets.sort(), ['Den@S1', 'Den@S2']);
  if (mode !== 'play') {
    assert.match(f.source.state.result.detail, /position 3/); assert.match(f.source.state.result.detail, /position 7/);
  }
});

for (const kind of ['playlist', 'favorite']) test(`dashboard ${kind} resolves each household's own native id and sends its shared title`, async () => {
  const f = await fixture(), review = await f.review('next', kind);
  assert.equal(new Set(review.targets.entries.map(e => e.item.id)).size, 2);
  assert.equal(await f.source.apply(review), true);
  assert.deepEqual(f.writes()[0].body, { zone: 'downstairs', [kind]: kind === 'favorite' ? 'Morning' : 'Evening', next: true });
  assert.equal(f.calls.filter(c => c.url.startsWith(kind === 'favorite' ? '/favorites?' : '/sonos/playlists?')).length, 4);
});

test('dashboard retains partial and all-failed receipts without repeating or guessing success', async () => {
  for (const allFailed of [false, true]) {
    const f = await fixture(), review = await f.review('next');
    f.write(() => {
      const result = f.batch(true);
      if (allFailed) {
        result.failed.push({ ...result.failed[0], household: 'S2', coordinator: 'fixture-S2-Den' });
        result.added = []; result.undoable = false;
      }
      return result;
    });
    assert.equal(await f.source.apply(review), true);
    assert.equal(f.source.state.result.partial, true); assert.equal(f.source.state.result.complete, false);
    assert.match(f.source.state.result.detail, /speaker refused/);
    assert.match(f.source.state.result.detail, /Do not repeat/);
    assert.equal(f.controller.state.uncertain, false); assert.equal(f.writes().length, 1);
  }
});

test('a changed group, source, queue or native catalog prevents every browser mutation', async () => {
  for (const change of [f => f.zones[1].members.push('New room'), f => { f.rows.get('Den@S1').track.uri = 'fixture:new'; },
    f => { f.queues.get('Den@S1').revision = 'changed'; }, f => { f.catalogs.get('Den@S1')[0].id = 'SQ:99'; }]) {
    const f = await fixture(), review = await f.review('next', 'playlist'); change(f);
    assert.equal(await f.source.apply(review), false); assert.equal(f.writes().length, 0);
    assert.equal(f.source.state.result.status, 'not_sent');
  }
});

test('missing household source fails review before a command; line-in remains one group', async () => {
  const f = await fixture(); f.source.open(); f.source.edit('destinationMode', 'set'); f.source.edit('destinationInput', 'downstairs');
  assert.equal(f.source.edit('sourceType', 'line_in'), false);
  f.source.edit('sourceType', 'playlist'); f.source.edit('nativeQuery', 'Evening'); f.catalogs.set('Den@S1', []);
  assert.equal(await f.source.review(), null); assert.equal(f.writes().length, 0);
  assert.match(f.source.state.message, /S1/);
});

test('existing dashboard command lane remains occupied through delivery and receipt', async () => {
  const f = await fixture(), review = await f.review('next'), reply = deferred();
  f.write(() => reply.promise);
  const action = f.source.apply(review); await until(() => f.writes().length === 1);
  assert.equal(f.controller.state.busy, true);
  assert.equal(await f.controller.command('resume'), false);
  assert.equal(f.source.edit('destinationInput', 'everywhere'), false);
  reply.resolve(f.batch(false)); assert.equal(await action, true);
  assert.equal(f.controller.state.busy, false); assert.equal(f.writes().length, 1);
});

test('malformed or lost batch replies remain uncertain and cannot be resubmitted', async () => {
  for (const malformed of [null, { done: 'Missing groups', complete: true, added: [], failed: [], logged: true, undoable: true }]) {
    const f = await fixture(), review = await f.review('next'); f.write(() => malformed);
    assert.equal(await f.source.apply(review), false); assert.equal(f.controller.state.uncertain, true);
    assert.equal(await f.source.apply(review), false); assert.equal(f.writes().length, 1);
  }
  const f = await fixture(), review = await f.review('next'); f.write(() => { throw new Error('Reply lost'); });
  assert.equal(await f.source.apply(review), false); assert.equal(f.source.state.result.status, 'uncertain');
  assert.equal(f.writes().length, 1);
});

test('room suggestions remain lazy and never select or execute a destination', async () => {
  const f = await fixture(); f.source.open(); assert.equal(f.calls.some(c => c.url === '/rooms'), false);
  assert.equal(await f.source.loadTargets(), true); assert.equal(f.source.state.destinationInput, '');
  assert.ok(f.source.state.suggestions.some(s => s.query === 'downstairs'));
  assert.equal(f.writes().length, 0);
});
