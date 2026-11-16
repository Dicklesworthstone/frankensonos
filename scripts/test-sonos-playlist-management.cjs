// Real phone/dashboard source models share exact native catalog edits while
// retaining the original household after a lost reply or a dialog change.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) { for (let i = 0; i < 300 && !check(); ++i) await Promise.resolve(); assert.ok(check()); }

async function fixture(kind, options = {}) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' }));
  const catalogs = new Map(zones.map(zone => ['Den@' + zone.household, [{ id: 'SQ:7', title: 'Saved & repeated' }, { id: 'SQ:8', title: 'Other list' }]]));
  const revisions = new Map(zones.map(zone => ['Den@' + zone.household, 'native:1']));
  const tracks = new Map(zones.map(zone => ['Den@' + zone.household, [
    { title: 'A saved track', uri: 'fixture:track' }, { title: 'Middle track', uri: 'fixture:middle' },
    { title: 'A saved track', uri: 'fixture:track' }]]));
  const calls = []; let reader, writer, api, controller, lanes = 0;
  const read = url => {
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/schedules') return [];
    if (parsed.pathname === '/sonos/playlists') return copy(catalogs.get(query.zone));
    if (parsed.pathname === '/sonos/playlists/contents') {
      if (query.expected_revision && query.expected_revision !== revisions.get(query.zone)) throw new Error('The saved playlist changed.');
      const item = catalogs.get(query.zone).find(item => item.id === query.playlist);
      if (!item) throw new Error('The saved playlist disappeared.');
      const rows = tracks.get(query.zone), offset = Number(query.offset || 0), limit = Math.min(Number(query.limit || 100), options.shortPage || 100);
      const items = rows.slice(offset, offset + limit).map((track, i) => ({ ...copy(track), position: offset + i + 1 }));
      const next = offset + items.length;
      return { zone: 'Den', playlist: copy(item), revision: revisions.get(query.zone), total: rows.length, offset,
        next_offset: next < rows.length ? next : null, items };
    }
    if (parsed.pathname.endsWith('/state')) {
      const target = decodeURIComponent(parsed.pathname.split('/')[2]), zone = zones.find(zone => 'Den@' + zone.household === target);
      return { zone: copy(zone), transport_state: 'paused', volume: 20, mute: false, track: { uri: 'fixture:playing', title: 'Owner song' } };
    }
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.ok(['/sonos/playlists/rename', '/sonos/playlists/remove', '/sonos/playlists/remove-tracks', '/sonos/playlists/move-tracks'].includes(url), 'only native catalog writes');
    assert.equal(body.expected_revision, revisions.get(body.zone));
    const catalog = catalogs.get(body.zone), index = catalog.findIndex(item => item.id === body.playlist);
    assert.notEqual(index, -1);
    const original = copy(catalog[index]), operation = url.split('/').at(-1).replace('-', '_');
    if (['remove_tracks', 'move_tracks'].includes(operation)) {
      const rows = tracks.get(body.zone), previous_total = rows.length;
      const changed = operation === 'remove_tracks' || body.insert_before < body.position || body.insert_before > body.position + body.count;
      if (changed) {
        const selected = rows.splice(body.position - 1, body.count);
        if (operation === 'move_tracks') rows.splice(body.insert_before - 1 - (body.insert_before > body.position ? body.count : 0), 0, ...selected);
        revisions.set(body.zone, 'native:2');
      }
      return { done: 'Saved tracks edited.', zone: 'Den', playlist: original, operation, position: body.position,
        count: body.count, ...(operation === 'move_tracks' ? { insert_before: body.insert_before } : {}),
        previous_total, total: rows.length, revision: revisions.get(body.zone), changed, logged: true };
    }
    const changed = operation === 'remove' || original.title !== body.title;
    if (operation === 'rename') catalog[index].title = body.title; else catalog.splice(index, 1);
    revisions.set(body.zone, 'native:2');
    return { done: operation === 'rename' ? 'Playlist renamed.' : 'Playlist removed.', zone: 'Den',
      playlist: operation === 'rename' ? copy(catalog[index]) : original, operation, changed, logged: true };
  };
  const call = async (url, body, config) => {
    calls.push({ url, body: copy(body), config });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  if (kind === 'phone') {
    api = Phone.model({ call(name, args = {}) {
      const [method, path] = Phone.routes[name] || (name === 'zones' ? ['GET', '/zones'] : ['GET', '/zones/{room}/state']);
      return call(path.replace('{room}', encodeURIComponent(args.params?.room)) + (args.query ? '?' + new URLSearchParams(args.query) : ''), method === 'POST' ? args.body : undefined);
    }, async execute(group, work) { ++lanes; return options.execute ? options.execute(group, work) : work(); } });
    await api.refresh(); api.edit('target', 'Den@S1');
  } else {
    controller = Core.model({ call, changed: () => api?.observe() });
    api = Dashboard.model({ call, controller });
    await controller.refresh(); controller.connection(true); api.open();
  }
  api.edit('sourceType', 'playlist'); await api.loadPlaylists(); api.edit('playlist', 'SQ:7'); calls.length = 0;
  return { api, controller, manager: api.manager, calls, catalogs, revisions, zones, tracks,
    writes: () => calls.filter(call => call.body !== undefined), lanes: () => lanes,
    read(fn) { reader = fn; }, write(fn) { writer = fn; },
    async otherHousehold() { if (kind === 'phone') api.edit('target', 'Den@S2'); else controller.select('Den@S2');
      await api.loadPlaylists(); api.edit('playlist', 'SQ:7'); },
    async reopen() { if (kind === 'phone') await api.refresh(); else api.open(); },
  };
}

for (const kind of ['phone', 'dashboard']) {
  for (const operation of ['rename', 'remove']) test(`${kind}: ${operation} is one reviewed catalog write in the selected household`, async () => {
    const f = await fixture(kind), originalOther = copy(f.catalogs.get('Den@S2'));
    f.manager.editTitle('  Dvořák & friends, <new>  ');
    const reviewed = await f.manager.review(operation);
    assert.equal(reviewed.total, 3); assert.equal(f.writes().length, 0);
    assert.equal(reviewed.item.id, 'SQ:7'); assert.equal(reviewed.group.target, 'Den@S1');
    assert.equal(await f.manager.confirm(reviewed), true);
    assert.equal(f.writes().length, 1);
    assert.deepEqual(f.writes()[0].body, { zone: 'Den@S1', playlist: 'SQ:7', expected_revision: 'native:1',
      ...(operation === 'rename' ? { title: 'Dvořák & friends, <new>' } : {}) });
    assert.equal(f.writes()[0].url, '/sonos/playlists/' + operation);
    assert.deepEqual(f.catalogs.get('Den@S2'), originalOther);
    assert.equal(f.manager.state.result.status, 'accepted'); assert.equal(f.api.state.playlist, '');
    assert.ok(f.calls.every(({ url }) => url === '/zones' || url.startsWith('/sonos/playlists')), 'catalog edits need no playback or queue I/O');
    assert.equal(await f.manager.confirm(reviewed), false);
    if (kind === 'phone') assert.equal(f.lanes(), 1);
  });

  test(`${kind}: a changed native revision or physical group prevents dispatch`, async () => {
    for (const change of [f => f.revisions.set('Den@S1', 'native:other'), f => f.zones[0].members.push('Kitchen')]) {
      const f = await fixture(kind), reviewed = await f.manager.review('remove'); change(f);
      assert.equal(await f.manager.confirm(reviewed), false);
      assert.equal(f.writes().length, 0); assert.equal(f.manager.state.result.status, 'not_sent');
    }
  });

  test(`${kind}: changing the source or household consumes captured deletion consent`, async () => {
    for (const change of [f => f.api.edit('playlist', 'SQ:8'), f => f.api.edit('sourceType', 'link'), f => f.otherHousehold()]) {
      const f = await fixture(kind), reviewed = await f.manager.review('remove'); await change(f);
      assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: closing during catalog preflight cannot dispatch or revive a review`, async () => {
    const f = await fixture(kind), gate = deferred(); f.read(() => gate.promise);
    const work = f.manager.review('remove'); await until(() => f.manager.state.reading);
    f.api.close(); gate.resolve(copy(f.zones)); assert.equal(await work, null);
    await f.reopen(); assert.equal(f.manager.state.pending, null); assert.equal(f.writes().length, 0);
  });

  test(`${kind}: late catalog receipts survive close and duplicate confirmations cannot replay`, async () => {
    const f = await fixture(kind), reviewed = await f.manager.review('remove'), gate = deferred();
    f.write(async (url, body, write) => { const result = write(url, body); await gate.promise; return result; });
    const work = f.manager.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.api.edit('playlist', 'SQ:8'), false);
    f.api.close(); gate.resolve(); assert.equal(await work, true);
    assert.equal(f.manager.state.result.status, 'accepted'); assert.equal(f.manager.state.result.group.target, 'Den@S1');
    assert.equal(f.writes().length, 1);
  });

  test(`${kind}: a lost reply requires explicit original-household recovery after switching groups`, async () => {
    const f = await fixture(kind), reviewed = await f.manager.review('remove');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost after persistence'); });
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.manager.state.result.status, 'uncertain');
    if (kind === 'dashboard') {
      await f.controller.refresh(); assert.equal(f.controller.state.uncertain, true);
      await f.controller.refreshStored('playlists', Phone.playlists, { explicit: true, storeTarget: 'Den@S2' });
      assert.equal(f.controller.state.uncertain, true);
    } else await f.api.refresh();
    f.api.close(); await f.reopen(); await f.otherHousehold();
    assert.equal(await f.manager.review('remove'), null);
    const catalog = await f.manager.recover(); assert.ok(catalog);
    assert.equal(f.calls.at(-1).url, '/sonos/playlists?zone=Den%40S1');
    assert.equal(f.manager.state.result.status, 'observed'); assert.match(f.manager.state.result.detail, /SQ:7 is absent/);
    assert.equal(f.manager.state.result.group.target, 'Den@S1'); assert.equal(f.manager.state.recovery, null);
    if (kind === 'dashboard') assert.equal(f.controller.state.uncertain, false);
    assert.equal(f.writes().length, 1);
  });

  test(`${kind}: malformed native receipts remain uncertain and cannot claim another playlist’s success`, async () => {
    for (const patch of [{ playlist: { id: 'SQ:8', title: 'Saved & repeated' } }, { zone: 'Kitchen' }, { operation: 'rename' }, { changed: false }, { logged: null }]) {
      const f = await fixture(kind), reviewed = await f.manager.review('remove');
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      assert.equal(await f.manager.confirm(reviewed), false);
      assert.equal(f.manager.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
      assert.equal(await f.manager.confirm(reviewed), false);
    }
  });

  test(`${kind}: catalog events fence recovery in its original household after selection moves`, async () => {
    const f = await fixture(kind), reviewed = await f.manager.review('remove');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost'); });
    await f.manager.confirm(reviewed); await f.otherHousehold();
    for (const household of ['S2', 'S1']) {
      const gate = deferred(); let captured;
      f.read((url, read) => { captured = read(url); return gate.promise; });
      const recovery = f.manager.recover(); await until(() => f.manager.state.reading);
      f.api.catalogChanged({ household, playlists: true, favorites: false });
      gate.resolve(captured);
      const result = await recovery;
      if (household === 'S2') {
        assert.ok(result, 'an unrelated household event does not cancel original catalog recovery');
        assert.equal(f.manager.state.recovery, null);
        // Start a separate uncertain edit so the next read exercises the same
        // original household after another selection change.
        f.read(null); if (kind === 'phone') f.api.edit('target', 'Den@S1'); else f.controller.select('Den@S1');
        f.catalogs.get('Den@S1').push({ id: 'SQ:7', title: 'Restored saved list' });
        await f.api.loadPlaylists(); f.api.edit('playlist', 'SQ:7');
        await f.manager.confirm(await f.manager.review('remove')); await f.otherHousehold();
      } else {
        assert.equal(result, null); assert.ok(f.manager.state.recovery, 'stale catalog evidence does not acknowledge an uncertain edit');
        if (kind === 'dashboard') assert.equal(f.controller.state.uncertain, true);
      }
    }
  });

  test(`${kind}: confirmed native changes stay successful when the separate action log fails`, async () => {
    const f = await fixture(kind); f.manager.editTitle('Renamed');
    f.write((url, body, write) => ({ ...write(url, body), logged: false }));
    assert.equal(await f.manager.confirm(await f.manager.review('rename')), true);
    assert.match(f.manager.state.result.detail, /do not repeat/i); assert.equal(f.manager.state.recovery, null);
    assert.equal(f.writes().length, 1);
  });

  test(`${kind}: a rename receipt must distinguish a changed title from an observed no-op`, async () => {
    for (const [title, changed] of [['Different title', false], ['Saved & repeated', true]]) {
      const f = await fixture(kind); f.manager.editTitle(title);
      f.write((url, body, write) => ({ ...write(url, body), changed }));
      const reviewed = await f.manager.review('rename');
      assert.equal(await f.manager.confirm(reviewed), false);
      assert.equal(f.manager.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
      assert.equal(await f.manager.confirm(reviewed), false);
    }
    const f = await fixture(kind); f.manager.editTitle('Saved & repeated');
    assert.equal(await f.manager.confirm(await f.manager.review('rename')), true);
    assert.equal(f.manager.state.result.status, 'accepted');
  });
}

test('native titles use Rust whitespace and UTF-8 limits before any catalog I/O', async () => {
  assert.equal(Phone.nativePlaylistTitle('\u0085Dvořák, <friends>\u3000'), 'Dvořák, <friends>');
  assert.equal(Phone.nativePlaylistTitle('\ufeff'), '\ufeff');
  assert.equal(Phone.nativePlaylistTitle('é'.repeat(511) + 'x').length, 512);
  const f = await fixture('phone');
  for (const title of ['', ' \t ', 'a\nb', 'é'.repeat(512), '\ud800']) {
    f.manager.editTitle(title); assert.equal(await f.manager.review('rename'), null);
  }
  assert.equal(f.calls.length, 0);
});

test('the phone native catalog preflight runs inside the same physical-group command lane', async () => {
  const f = await fixture('phone', { execute() { throw new Error('The physical group is busy'); } });
  const reviewed = await f.manager.review('remove'), reads = f.calls.length;
  assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  assert.equal(f.calls.length, reads); assert.equal(f.manager.state.result.status, 'not_sent');
});

test('the Listening room native edit cannot bypass another stored command', async () => {
  const f = await fixture('dashboard'), reviewed = await f.manager.review('remove'), gate = deferred();
  const work = f.controller.mutate(async () => { await gate.promise; return { done: 'Stored', changed: true }; }, { stored: 'schedules' });
  assert.equal(await f.manager.confirm(reviewed), false); gate.resolve(); await work;
  assert.equal(f.writes().length, 0);
});

test('shared native management controls render an explicit destructive review as text', async () => {
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.events = new Map(); this.attributes = {}; this.value = ''; this.textContent = ''; }
    append(...nodes) { this.children.push(...nodes); }
    replaceChildren(...nodes) { this.children = nodes; }
    setAttribute(key, value) { this.attributes[key] = value; }
    addEventListener(key, fn) { this.events.set(key, fn); }
    set innerHTML(_) { assert.fail('Metadata is text, never markup'); }
  }
  const f = await fixture('phone'); f.catalogs.get('Den@S1')[0].title = '<script>Saved & repeated</script>';
  await f.api.loadPlaylists(); f.api.edit('playlist', 'SQ:7');
  const panel = Phone.playlistManagerPanel(f.manager, { createElement: tag => new Element(tag) });
  const descendants = node => [node, ...node.children.flatMap(descendants)];
  panel.render(true);
  const remove = descendants(panel.node).find(node => node.textContent === 'Review deletion'); remove.events.get('click')();
  await until(() => !!f.manager.state.pending); panel.render(true);
  const texts = descendants(panel.node).map(node => node.textContent).join(' ');
  assert.match(texts, /<script>Saved & repeated<\/script>/); assert.match(texts, /cannot be undone here/);
  assert.match(texts, /3 track\(s\)/); assert.equal(f.writes().length, 0);
  descendants(panel.node).find(node => node.textContent === 'Delete saved playlist').events.get('click')();
  await until(() => f.manager.state.result?.status === 'accepted'); assert.equal(f.writes().length, 1);
});

function range(manager, position, count, before) {
  assert.equal(manager.editTracks('position', String(position)), true);
  assert.equal(manager.editTracks('count', String(count)), true);
  if (before !== undefined) assert.equal(manager.editTracks('insert_before', String(before)), true);
}

for (const kind of ['phone', 'dashboard']) {
  test(`${kind}: remove one repeated saved occurrence without deleting its other copy`, async () => {
    const f = await fixture(kind), other = copy(f.tracks.get('Den@S2'));
    range(f.manager, 3, 1);
    const review = await f.manager.review('remove_tracks');
    assert.deepEqual(review.tracks.map(item => [item.position, item.title]), [[3, 'A saved track']]);
    assert.equal(f.writes().length, 0);
    assert.equal(await f.manager.confirm(review), true);
    assert.deepEqual(f.tracks.get('Den@S1').map(item => item.uri), ['fixture:track', 'fixture:middle']);
    assert.deepEqual(f.tracks.get('Den@S2'), other);
    assert.deepEqual(f.writes().map(call => [call.url, call.body]), [['/sonos/playlists/remove-tracks', {
      zone: 'Den@S1', playlist: 'SQ:7', expected_revision: 'native:1', position: 3, count: 1,
    }]]);
    assert.ok(f.calls.every(({ url }) => url === '/zones' || url.startsWith('/sonos/playlists')));
    assert.equal(await f.manager.confirm(review), false);
  });

  for (const [position, count, before, order] of [[1, 2, 6, 'cdeab'], [4, 2, 1, 'deabc'], [2, 2, 3, 'abcde'], [2, 2, 4, 'abcde']]) {
    test(`${kind}: move saved range ${position}+${count} before original ${before}`, async () => {
      const f = await fixture(kind);
      f.tracks.set('Den@S1', [...'abcde'].map(title => ({ title, uri: 'fixture:' + title })));
      range(f.manager, position, count, before);
      const review = await f.manager.review('move_tracks');
      assert.equal(review.anchor?.position ?? 6, before);
      assert.equal(await f.manager.confirm(review), true);
      assert.equal(f.tracks.get('Den@S1').map(item => item.title).join(''), order);
      assert.equal(f.writes().length, 1); assert.equal(f.writes()[0].body.insert_before, before);
      assert.equal(f.manager.state.result.status, 'accepted');
    });
  }

  test(`${kind}: remove every saved occurrence leaves an empty named playlist`, async () => {
    const f = await fixture(kind); range(f.manager, 1, 3);
    assert.equal(await f.manager.confirm(await f.manager.review('remove_tracks')), true);
    assert.equal(f.tracks.get('Den@S1').length, 0);
    assert.ok(f.catalogs.get('Den@S1').some(item => item.id === 'SQ:7'));
  });

  test(`${kind}: short native pages retain and recheck the complete reviewed range`, async () => {
    const f = await fixture(kind, { shortPage: 2 });
    f.tracks.set('Den@S1', [...'abcdef'].map(title => ({ title, uri: 'fixture:' + title })));
    range(f.manager, 2, 4, 1);
    const review = await f.manager.review('move_tracks');
    assert.deepEqual(review.tracks.map(item => item.title), [...'bcde']);
    assert.equal(review.anchor.title, 'a');
    assert.equal(await f.manager.confirm(review), true);
    assert.equal(f.tracks.get('Den@S1').map(item => item.title).join(''), 'bcdeaf');
    assert.equal(f.calls.filter(call => call.url.includes('offset=3')).length, 2);
  });

  test(`${kind}: changed selected metadata or destination invalidates track-edit consent`, async () => {
    for (const position of [1, 3]) {
      const f = await fixture(kind); range(f.manager, 1, 1, 3);
      const review = await f.manager.review('move_tracks');
      f.tracks.get('Den@S1')[position - 1].title = 'Other occurrence';
      assert.equal(await f.manager.confirm(review), false);
      assert.equal(f.writes().length, 0); assert.equal(f.manager.state.result.status, 'not_sent');
    }
  });

  test(`${kind}: selecting a displayed occurrence retains that contents revision`, async () => {
    const f = await fixture(kind); await f.api.loadContents();
    assert.equal(f.manager.chooseTrack(3, f.api.state.contents), true);
    f.manager.editTracks('count', '1');
    f.revisions.set('Den@S1', 'native:changed');
    assert.equal(await f.manager.review('remove_tracks'), null);
    assert.equal(f.writes().length, 0);
  });

  test(`${kind}: editing a range or receiving a catalog event retires its review`, async () => {
    for (const change of [f => f.manager.editTracks('count', '2'), f => f.api.catalogChanged({ household: 'S1', playlists: true })]) {
      const f = await fixture(kind); range(f.manager, 1, 1);
      const review = await f.manager.review('remove_tracks'); change(f);
      assert.equal(await f.manager.confirm(review), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: uncertain track edit observes the original household without replay`, async () => {
    const f = await fixture(kind); range(f.manager, 2, 1);
    const review = await f.manager.review('remove_tracks');
    f.write((url, body, write) => { write(url, body); throw new Error('Reply lost'); });
    assert.equal(await f.manager.confirm(review), false);
    assert.equal(f.manager.state.result.status, 'uncertain');
    await f.otherHousehold(); assert.equal(await f.manager.review('remove_tracks'), null);
    assert.ok(await f.manager.recover());
    assert.equal(f.calls.at(-1).url, '/sonos/playlists?zone=Den%40S1');
    assert.equal(f.manager.state.result.status, 'observed'); assert.equal(f.writes().length, 1);
    assert.deepEqual(f.tracks.get('Den@S1').map(item => item.uri), ['fixture:track', 'fixture:track']);
    if (kind === 'dashboard') assert.equal(f.controller.state.uncertain, false);
  });

  test(`${kind}: contradictory track-edit receipts cannot report a confirmed result`, async () => {
    for (const patch of [{ position: 1 }, { count: 2 }, { previous_total: 8 }, { total: 3 }, { revision: 'native:1' },
      { revision: '' }, { insert_before: 1 }, { changed: false }, { logged: null }]) {
      const f = await fixture(kind); range(f.manager, 2, 1);
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      const review = await f.manager.review('remove_tracks');
      assert.equal(await f.manager.confirm(review), false, JSON.stringify(patch));
      assert.equal(f.manager.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
      assert.equal(await f.manager.confirm(review), false);
    }
  });
}

test('invalid saved-track ranges fail before any catalog I/O or mutation', async () => {
  for (const [position, count, before] of [['0', '1', '2'], ['1', '0', '2'], ['1.5', '1', '2'],
    ['1e2', '1', '2'], ['1000', '2', '2'], ['1', '1', '1002'], ['1', '1', '-1']]) {
    const f = await fixture('phone'); range(f.manager, position, count, before);
    assert.equal(await f.manager.review('move_tracks'), null); assert.equal(f.calls.length, 0);
  }
  for (const [position, count, before] of [[3, 2, 1], [1, 1, 5]]) {
    const f = await fixture('phone'); range(f.manager, position, count, before);
    assert.equal(await f.manager.review('move_tracks'), null); assert.equal(f.writes().length, 0);
  }
  const f = await fixture('phone');
  f.tracks.set('Den@S1', Array.from({ length: 1001 }, (_, i) => ({ title: String(i) })));
  assert.equal(await f.manager.review('remove_tracks'), null); assert.equal(f.writes().length, 0);
});

test('both remotes share a track-edit review showing exact occurrences and destination as text', async () => {
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.events = new Map(); this.attributes = {}; this.value = ''; this.textContent = ''; }
    append(...nodes) { this.children.push(...nodes); }
    replaceChildren(...nodes) { this.children = nodes; }
    setAttribute(key, value) { this.attributes[key] = value; }
    addEventListener(key, fn) { this.events.set(key, fn); }
    set innerHTML(_) { assert.fail('Track metadata must be text'); }
  }
  const f = await fixture('phone'); f.tracks.get('Den@S1')[1].title = '<script>Middle & selected</script>';
  const panel = Phone.playlistManagerPanel(f.manager, { createElement: tag => new Element(tag) });
  const descendants = node => [node, ...node.children.flatMap(descendants)];
  range(f.manager, 2, 1, 1); panel.render(true);
  descendants(panel.node).find(node => node.textContent === 'Review track move').events.get('click')();
  await until(() => !!f.manager.state.pending); panel.render(true);
  const texts = descendants(panel.node).map(node => node.textContent).join(' ');
  assert.match(texts, /Track 2: <script>Middle & selected<\/script>/);
  assert.match(texts, /before original track 1: A saved track/);
  assert.match(texts, /Current playback, loaded queues and the DJ continue unchanged/);
  assert.equal(f.writes().length, 0);
  descendants(panel.node).find(node => node.textContent === 'Move saved tracks').events.get('click')();
  await until(() => f.manager.state.result?.status === 'accepted');
  assert.equal(f.tracks.get('Den@S1')[0].title, '<script>Middle & selected</script>');
});
