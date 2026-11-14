'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const F = require('../crates/fsonos-api/src/remote/favorites.js');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) { for (let i = 0; i < 200 && !check(); ++i) await Promise.resolve(); assert.ok(check()); }

async function fixture(kind = 'phone') {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den', 'Side'], transport_state: 'playing' }));
  const original = [{ id: 'FV:2/7', title: 'Evening radio', kind: 'stream', description: 'The saved station' },
    { id: 'FV:2/8', title: 'Evening radio', kind: 'track', description: 'A different entry with the same name' },
    { id: 'FV:2/9', title: 'Old unavailable music', kind: 'unplayable' }];
  const catalogs = new Map(zones.map(zone => ['Den@' + zone.household, copy(original)]));
  const sources = new Map(zones.map(zone => ['Den@' + zone.household, { id: '', title: 'Actual station', kind: 'stream', description: 'Station, not the transient song title' }]));
  const versions = new Map(zones.map(zone => ['Den@' + zone.household, 'revision:1']));
  const calls = []; let reader, writer, controller, api, externalBusy = false, lanes = 0, held = 0;
  const read = url => {
    const parsed = new URL(url, 'http://fixture'), q = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname.endsWith('/state')) {
      const target = decodeURIComponent(parsed.pathname.split('/')[2]);
      return { zone: copy(zones.find(zone => 'Den@' + zone.household === target)), transport_state: 'playing', volume: 20, mute: false,
        track: { uri: 'fixture:transient-song', title: 'Transient radio song' } };
    }
    if (parsed.pathname === '/favorites') return copy(catalogs.get(q.zone));
    if (parsed.pathname === '/favorites/current') return { zone: 'Den', favorite: copy(sources.get(q.zone)), revision: versions.get(q.zone) };
    if (parsed.pathname === '/favorites/inspect') {
      const item = catalogs.get(q.zone).find(item => item.id === q.favorite);
      if (!item) throw new Error('This exact favorite is no longer present');
      return { zone: 'Den', favorite: copy(item), revision: versions.get(q.zone) };
    }
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.ok(['/favorites/save', '/favorites/rename', '/favorites/remove'].includes(url));
    assert.equal(body.expected_revision, versions.get(body.zone));
    const operation = url.split('/').at(-1), rows = catalogs.get(body.zone);
    let favorite, changed = true;
    if (operation === 'save') { favorite = { ...sources.get(body.zone), id: 'FV:2/10', title: body.title }; rows.push(copy(favorite)); }
    else {
      const index = rows.findIndex(item => item.id === body.favorite); assert.notEqual(index, -1);
      favorite = copy(rows[index]);
      if (operation === 'rename') { changed = favorite.title !== body.title; favorite.title = body.title; rows[index] = copy(favorite); }
      else rows.splice(index, 1);
    }
    versions.set(body.zone, 'revision:2');
    return { done: 'Favorite ' + operation + ' confirmed.', zone: 'Den', favorite, operation, changed, logged: true };
  };
  const call = async (url, body) => {
    calls.push({ url, body: copy(body) });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  if (kind === 'dashboard') {
    controller = C.model({ call, now: () => 1000, changed: () => api?.observe() });
    api = F.model({ call, selection: () => controller.selected() ? P.groups([controller.selected().zone])[0] : null,
      available: () => controller.state.topologyFresh && controller.canControl(), readable: () => controller.state.active && !controller.state.busy,
      async commit(reviewed, work) {
        ++lanes; ++held;
        try { if (!await controller.mutate(work, { stored: 'favorites', storeTarget: reviewed.group.target })) throw new Error(controller.state.message); }
        finally { --held; }
      }, readCatalog: (storeTarget, decode) => controller.refreshStored('favorites', decode, { explicit: true, storeTarget }),
    });
    await controller.refresh(); controller.connection(true);
  } else {
    const phone = F.phoneClient((name, args = {}) => {
      const [method, route] = F.routes[name];
      return call(route + (args.query && Object.keys(args.query).length ? '?' + new URLSearchParams(args.query) : ''), method === 'POST' ? args.body : undefined);
    });
    api = F.model({ call: phone, available: () => !externalBusy,
      async commit(reviewed, work) {
        if (externalBusy) throw new Error('Another command is using this group');
        ++lanes; ++held;
        try { return await work({ check() { if (externalBusy) throw new Error('Group command lane changed'); }, send: phone }); }
        finally { --held; }
      },
    });
  }
  await api.open('Den@S1'); calls.length = 0;
  return { api, controller, calls, zones, catalogs, sources, versions,
    writes: () => calls.filter(call => call.body !== undefined), lanes: () => lanes, held: () => held,
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, busy(value) { externalBusy = value; api.observe(); },
    select(id = 'FV:2/7') { assert.equal(api.select(api.state.catalog.find(item => item.id === id)), true); },
    async review(operation, title = 'Renamed & kept') {
      if (operation === 'save') assert.equal(await api.readCurrent(), true); else this.select();
      api.editTitle(title); const result = await api.review(operation); assert.ok(result); return result;
    },
    async switchGroup() { if (controller) controller.select('Den@S2'); return api.refresh('Den@S2'); },
  };
}

for (const kind of ['phone', 'dashboard']) {
  for (const operation of ['save', 'rename', 'remove']) test(`${kind}: ${operation} uses one exact original-household request and no playback I/O`, async () => {
    const f = await fixture(kind), other = copy(f.catalogs.get('Den@S2'));
    const review = await f.review(operation, '  Dvořák & <friends>  ');
    assert.equal(f.writes().length, 0); assert.equal(review.group.target, 'Den@S1');
    if (operation === 'save') { assert.equal(review.item.title, 'Actual station'); assert.equal(review.item.id, ''); }
    else assert.equal(review.item.id, 'FV:2/7');
    assert.equal(await f.api.confirm(review), true); assert.equal(f.writes().length, 1);
    assert.deepEqual(f.writes()[0], { url: '/favorites/' + operation, body: { zone: 'Den@S1', expected_revision: 'revision:1',
      ...(operation === 'save' ? {} : { favorite: 'FV:2/7' }), ...(operation === 'remove' ? {} : { title: 'Dvořák & <friends>' }) } });
    assert.equal(f.api.state.result.status, 'accepted'); assert.equal(await f.api.confirm(review), false);
    assert.deepEqual(f.catalogs.get('Den@S2'), other); assert.equal(f.lanes(), 1); assert.equal(f.held(), 0);
    assert.ok(f.calls.every(({ url }) => url === '/zones' || url.startsWith('/favorites')), 'authoring never changes transport or queue');
  });

  test(`${kind}: source changes before review and before confirmation prevent saving`, async () => {
    for (const afterReview of [false, true]) {
      const f = await fixture(kind); await f.api.readCurrent();
      const review = afterReview ? await f.api.review('save') : null;
      f.sources.get('Den@S1').title = 'A new selected song'; f.versions.set('Den@S1', 'revision:new');
      if (afterReview) assert.equal(await f.api.confirm(review), false);
      else assert.equal(await f.api.review('save'), null);
      assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: exact item metadata, revision and original group are rechecked inside admission`, async () => {
    for (const change of [f => f.catalogs.get('Den@S1')[0].title = 'Renamed elsewhere',
      f => f.versions.set('Den@S1', 'same-title-new-metadata'), f => f.zones[0].members.push('Kitchen')]) {
      const f = await fixture(kind), review = await f.review('remove'); change(f);
      assert.equal(await f.api.confirm(review), false); assert.equal(f.writes().length, 0);
      assert.equal(f.api.state.result.status, 'not_sent'); assert.equal(f.held(), 0);
    }
  });

  test(`${kind}: switching households or editing a draft consumes old consent`, async () => {
    for (const change of [f => f.switchGroup(), f => f.api.editTitle('A different title'), f => f.api.close()]) {
      const f = await fixture(kind), review = await f.review('rename'); await change(f);
      assert.equal(await f.api.confirm(review), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: closing while final observation is pending sends no write`, async () => {
    const f = await fixture(kind), review = await f.review('save'), gate = deferred();
    f.read((url, read) => url.startsWith('/favorites/current') ? gate.promise : read(url));
    const applying = f.api.confirm(review); await until(() => f.calls.some(row => row.url.startsWith('/favorites/current') && f.held() === 1));
    f.api.close(); gate.resolve({ zone: 'Den', favorite: copy(f.sources.get('Den@S1')), revision: 'revision:1' });
    assert.equal(await applying, false); assert.equal(f.writes().length, 0); assert.equal(f.held(), 0);
  });

  test(`${kind}: late confirmed receipts survive close while the one command lane stays held`, async () => {
    const f = await fixture(kind), review = await f.review('remove'), gate = deferred();
    f.write(async (url, body, write) => { const result = write(url, body); await gate.promise; return result; });
    const applying = f.api.confirm(review); await until(() => f.writes().length === 1);
    f.api.close(); assert.equal(f.held(), 1); assert.equal(await f.api.confirm(review), false);
    gate.resolve(); assert.equal(await applying, true); assert.equal(f.held(), 0);
    await f.api.open(); assert.equal(f.api.state.result.status, 'accepted'); assert.equal(f.writes().length, 1);
  });

  test(`${kind}: lost save reply persists through reopen and another household until explicit original catalog recovery`, async () => {
    const f = await fixture(kind), review = await f.review('save', 'New favorite');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost after native save'); });
    assert.equal(await f.api.confirm(review), false); assert.equal(f.api.state.result.status, 'uncertain');
    f.api.close(); await f.api.open();
    if (f.controller) {
      await f.controller.refresh(true); assert.equal(f.controller.state.uncertain, true, 'a playback read is not a favorites receipt');
      await f.controller.refreshStored('favorites', F.catalog, { explicit: true, storeTarget: 'Den@S2' });
      assert.equal(f.controller.state.uncertain, true, 'the wrong household does not acknowledge the write');
    }
    await f.switchGroup(); assert.equal(await f.api.readCurrent(), false);
    const recovered = await f.api.recover(); assert.ok(recovered); assert.equal(f.calls.at(-1).url, '/favorites?zone=Den%40S1');
    assert.equal(f.api.state.result.status, 'observed'); assert.equal(f.api.state.result.group.target, 'Den@S1');
    assert.match(f.api.state.result.detail, /1 matching favorite/); assert.match(f.api.state.result.detail, /does not prove/);
    assert.equal(f.api.state.recovery, null); assert.equal(f.writes().length, 1);
    if (f.controller) assert.equal(f.controller.state.uncertain, false);
  });

  test(`${kind}: recovery catalog events belong to the original household after selection changes`, async () => {
    const f = await fixture(kind), review = await f.review('remove');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost'); });
    await f.api.confirm(review); await f.switchGroup();
    const gate = deferred(); let captured;
    f.read((url, read) => { const result = read(url); if (url.startsWith('/favorites?')) { captured = result; return gate.promise; } return result; });
    const recovery = f.api.recover(); await until(() => captured);
    f.api.catalogChanged({ household: 'S1', favorites: true, playlists: false }); gate.resolve(captured);
    assert.equal(await recovery, null); assert.ok(f.api.state.recovery); assert.equal(f.writes().length, 1);
    if (f.controller) assert.equal(f.controller.state.uncertain, true);
  });

  test(`${kind}: an unrelated household event does not cancel explicit original-household recovery`, async () => {
    const f = await fixture(kind), review = await f.review('remove');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost'); });
    await f.api.confirm(review); await f.switchGroup();
    const gate = deferred(); let captured;
    f.read((url, read) => { const result = read(url); if (url.startsWith('/favorites?')) { captured = result; return gate.promise; } return result; });
    const recovery = f.api.recover(); await until(() => captured);
    f.api.catalogChanged({ household: 'S2', favorites: true, playlists: false }); gate.resolve(captured);
    assert.ok(await recovery); assert.equal(f.api.state.recovery, null);
    assert.equal(f.api.state.result.group.target, 'Den@S1'); assert.equal(f.writes().length, 1);
  });

  test(`${kind}: recovery keeps the original room address after it becomes another coordinator's member`, async () => {
    const f = await fixture(kind), review = await f.review('save', 'Retained station');
    f.write((url, body, write) => { write(url, body); throw new Error('Native save reply lost'); });
    await f.api.confirm(review);
    f.zones[0].coordinator_room = 'Side';
    if (f.controller) {
      await f.controller.refresh(); assert.equal(f.controller.state.uncertain, true);
      f.controller.select('Den@S2');
    }
    const rows = await f.api.recover(); assert.ok(rows);
    assert.equal(f.calls.at(-1).url, '/favorites?zone=Den%40S1');
    assert.equal(f.api.state.result.group.target, 'Den@S1'); assert.equal(f.api.state.recovery, null);
    assert.equal(f.writes().length, 1);
    if (f.controller) assert.equal(f.controller.state.uncertain, false);
  });

  test(`${kind}: unplayable saved favorites remain manageable and a matching-title rename is a verified no-op`, async () => {
    const f = await fixture(kind); f.select('FV:2/9');
    const review = await f.api.review('rename'); assert.equal(await f.api.confirm(review), true);
    assert.equal(f.api.state.result.favorite.id, 'FV:2/9'); assert.equal(f.api.state.result.status, 'accepted');
  });

  test(`${kind}: malformed mutation receipts remain uncertain without claiming another entry`, async () => {
    for (const patch of [{ operation: 'rename' }, { zone: 'Other' }, { changed: false }, { logged: null },
      { favorite: { id: 'FV:2/8', title: 'Evening radio', kind: 'stream' } }]) {
      const f = await fixture(kind), review = await f.review('remove');
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      assert.equal(await f.api.confirm(review), false); assert.equal(f.api.state.result.status, 'uncertain');
      assert.equal(await f.api.confirm(review), false); assert.equal(f.writes().length, 1);
    }
  });

  test(`${kind}: a failed separate action log preserves the native success`, async () => {
    const f = await fixture(kind), review = await f.review('rename');
    f.write((url, body, write) => ({ ...write(url, body), logged: false }));
    assert.equal(await f.api.confirm(review), true); assert.equal(f.api.state.result.status, 'accepted');
    assert.match(f.api.state.result.detail, /Do not repeat/); assert.equal(f.api.state.recovery, null);
  });
}

test('favorite title validation matches UTF-8 and Rust whitespace bounds before reads', async () => {
  assert.equal(F.titleOf('\u0085Dvořák\u3000'), 'Dvořák'); assert.equal(F.titleOf('\ufeff'), '\ufeff');
  assert.equal(F.titleOf('é'.repeat(511) + 'x').length, 512);
  const f = await fixture(); f.select();
  for (const title of ['', ' \t ', 'a\nb', 'é'.repeat(512), '\ud800']) {
    f.api.editTitle(title); assert.equal(await f.api.review('rename'), null);
  }
  assert.equal(f.calls.length, 0);
});

test('malformed previews and duplicate saved IDs never become actionable observations', async () => {
  const f = await fixture();
  f.read((url, read) => url.startsWith('/favorites/current') ? { zone: 'Den', revision: 'bad', favorite: { id: 'FV:2/1', title: 'Not a current preview', kind: 'stream' } } : read(url));
  assert.equal(await f.api.readCurrent(), false); assert.equal(await f.api.review('save'), null);
  assert.throws(() => F.catalog([{ id: 'FV:2/7', title: 'One', kind: 'track' }, { id: 'FV:2/7', title: 'Two', kind: 'stream' }]), /repeats/);
  assert.equal(f.writes().length, 0);
});

test('phone command contention consumes an old review without dispatch', async () => {
  const f = await fixture(), review = await f.review('rename'); f.busy(true);
  assert.equal(f.api.state.pending, null); assert.equal(await f.api.confirm(review), false);
  assert.equal(f.writes().length, 0); assert.equal(f.lanes(), 0);
});

test('phone adapter exposes only the native favorites routes', () => {
  const calls = [], client = F.phoneClient((...args) => calls.push(args));
  assert.throws(() => client('/pause', { zone: 'Den@S1' }), /Unsupported/);
  assert.throws(() => client('/favorites/save?repeat=1', {}), /Unsupported/);
  assert.throws(() => client('https://other.invalid/favorites'), /Unsupported/);
  assert.equal(calls.length, 0);
});
