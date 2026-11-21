// Execute both production remote models: a saved-playlist append copies
// reviewed native occurrences and never sends a playback or live-queue write.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) { for (let i = 0; i < 300 && !check(); ++i) await Promise.resolve(); assert.ok(check()); }

async function fixture(kind) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' }));
  const catalogs = new Map(), lists = new Map(), queues = new Map();
  for (const zone of zones) {
    const target = 'Den@' + zone.household;
    catalogs.set(target, [{ id: 'SQ:7', title: 'Destination <saved>' }, { id: 'SQ:8', title: 'Other & source' }]);
    for (const item of catalogs.get(target)) lists.set(target + '/' + item.id, { revision: zone.household + ':' + item.id + ':1', rows: [
      { title: 'First <track>', creator: 'Artist & friends', uri: 'x-file-cifs://example.invalid/first.flac' },
      { title: 'Second track', creator: 'Artist', uri: 'x-file-cifs://example.invalid/repeated.flac' },
      { title: 'Second track', creator: 'Artist', uri: 'x-file-cifs://example.invalid/repeated.flac' },
    ] });
    queues.set(target, { revision: 'queue:' + zone.household, rows: [
      { title: 'Queue first', uri: 'x-file-cifs://example.invalid/queue-first.flac' },
      { title: 'Queue second', uri: 'x-file-cifs://example.invalid/queue-second.flac' },
    ] });
  }
  const calls = []; let reader, writer, api, controller;
  const read = url => {
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/schedules') return [];
    if (parsed.pathname === '/sonos/playlists') return copy(catalogs.get(query.zone));
    if (parsed.pathname === '/sonos/playlists/contents') {
      const list = lists.get(query.zone + '/' + query.playlist), item = catalogs.get(query.zone).find(item => item.id === query.playlist);
      if (!item || !list) throw new Error('The saved playlist disappeared.');
      if (query.expected_revision && query.expected_revision !== list.revision) throw new Error('The saved playlist changed.');
      const offset = Number(query.offset || 0), limit = Number(query.limit || 100);
      const items = list.rows.slice(offset, offset + limit).map((track, i) => ({ ...copy(track), position: offset + i + 1 }));
      const next = offset + items.length;
      return { zone: 'Den', playlist: copy(item), revision: list.revision, total: list.rows.length, offset,
        next_offset: next < list.rows.length ? next : null, items };
    }
    const target = decodeURIComponent(parsed.pathname.split('/')[2] || '');
    if (parsed.pathname.endsWith('/state')) return { zone: copy(zones.find(zone => 'Den@' + zone.household === target)),
      transport_state: 'paused', volume: 20, mute: false, track: { uri: 'fixture:playing', title: 'Owner song' } };
    if (parsed.pathname.endsWith('/queue')) {
      const queue = queues.get(target), offset = Number(query.offset || 0), limit = Number(query.limit || 100);
      return { zone: 'Den', revision: queue.revision, total: queue.rows.length,
        items: queue.rows.slice(offset, offset + limit).map((track, i) => ({ ...copy(track), position: offset + i + 1 })) };
    }
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body, count = body.positions.length) => {
    assert.equal(url, '/sonos/playlists/append-tracks', 'only append to the existing saved playlist');
    const list = lists.get(body.zone + '/' + body.playlist);
    const source = body.source_playlist ? lists.get(body.zone + '/' + body.source_playlist) : queues.get(body.zone);
    assert.equal(body.expected_revision, list.revision); assert.equal(body.source_revision, source.revision);
    const selected = body.positions.map(position => copy(source.rows[position - 1])), previous_total = list.rows.length;
    list.rows.push(...selected.slice(0, count)); list.revision += ':next';
    return { done: `Added ${count} selected tracks.`, zone: 'Den', playlist: copy(catalogs.get(body.zone).find(item => item.id === body.playlist)),
      revision: list.revision, previous_total, total: list.rows.length, added: count,
      positions: [...body.positions], titles: selected.map(track => track.title), logged: true };
  };
  const call = async (url, body, config) => {
    calls.push({ url, body: copy(body), config });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  if (kind === 'phone') {
    api = Phone.model({ call(name, args = {}) {
      const [method, path] = Phone.routes[name] || (name === 'zones' ? ['GET', '/zones'] : ['GET', '/zones/{room}/state']);
      return call(path.replace('{room}', encodeURIComponent(args.params?.room)) + (args.query ? '?' + new URLSearchParams(args.query) : ''), method === 'POST' ? args.body : undefined);
    } });
    await api.refresh(); api.edit('target', 'Den@S1');
  } else {
    controller = Core.model({ call, changed: () => api?.observe() });
    api = Dashboard.model({ call, controller });
    await controller.refresh(); controller.connection(true); api.open();
  }
  api.edit('sourceType', 'playlist'); await api.loadPlaylists(); api.edit('playlist', 'SQ:7'); calls.length = 0;
  const manager = api.manager;
  return { api, controller, manager, calls, zones, catalogs, lists, queues,
    writes: () => calls.filter(call => call.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; },
    async review(source = '', positions = '2,1,2') {
      assert.equal(manager.editAppend('sourcePlaylist', source), true);
      assert.equal(manager.editAppend('sourcePositions', positions), true);
      return manager.review('append_tracks');
    },
    async otherHousehold() { if (kind === 'phone') api.edit('target', 'Den@S2'); else controller.select('Den@S2');
      await api.loadPlaylists(); api.edit('playlist', 'SQ:7'); },
    async reopen() { if (kind === 'phone') await api.refresh(); else api.open(); },
  };
}

for (const kind of ['phone', 'dashboard']) {
  for (const source of ['', 'SQ:8', 'SQ:7']) test(`${kind}: copy ${source || 'queue'} occurrences in order with repeats, keeping playback untouched`, async () => {
    const f = await fixture(kind), queues = copy(f.queues), other = copy(f.lists.get('Den@S2/SQ:7'));
    const before = copy(f.lists.get('Den@S1/SQ:7')), input = copy(source ? f.lists.get('Den@S1/' + source) : f.queues.get('Den@S1'));
    const reviewed = await f.review(source);
    assert.ok(reviewed); assert.equal(f.writes().length, 0);
    assert.deepEqual(reviewed.source.tracks.map(track => track.title), [input.rows[1].title, input.rows[0].title, input.rows[1].title]);
    assert.ok(Object.isFrozen(reviewed)); assert.ok(Object.isFrozen(reviewed.body));
    assert.equal(await f.manager.confirm(reviewed), true);
    assert.equal(await f.manager.confirm(reviewed), false);
    assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sonos/playlists/append-tracks', body: {
      zone: 'Den@S1', playlist: 'SQ:7', expected_revision: before.revision, source_revision: input.revision,
      positions: [2, 1, 2], ...(source ? { source_playlist: source } : {}),
    } }]);
    assert.deepEqual(f.lists.get('Den@S1/SQ:7').rows, [...before.rows, input.rows[1], input.rows[0], input.rows[1]]);
    assert.deepEqual(f.queues, queues); assert.deepEqual(f.lists.get('Den@S2/SQ:7'), other);
    assert.equal(f.manager.state.result.status, 'accepted');
    if (source === 'SQ:8') assert.deepEqual(f.lists.get('Den@S1/SQ:8'), input);
  });

  test(`${kind}: append to an observed empty playlist keeps its native identity`, async () => {
    const f = await fixture(kind); f.lists.get('Den@S1/SQ:7').rows = [];
    assert.equal(await f.manager.confirm(await f.review()), true);
    assert.equal(f.lists.get('Den@S1/SQ:7').rows.length, 3);
    assert.equal(f.catalogs.get('Den@S1')[0].id, 'SQ:7');
  });

  test(`${kind}: changed source revision, destination revision, group or source metadata prevents dispatch`, async () => {
    for (const change of [
      f => { f.queues.get('Den@S1').revision += ':changed'; },
      f => { f.lists.get('Den@S1/SQ:7').revision += ':changed'; },
      f => { f.queues.get('Den@S1').rows[1].title = 'Substituted song'; },
      f => { f.queues.get('Den@S1').rows[1].uri = 'x-file-cifs://example.invalid/substitute.flac'; },
      f => { f.zones[0].members.push('Kitchen'); },
    ]) {
      const f = await fixture(kind), reviewed = await f.review(); change(f);
      assert.equal(await f.manager.confirm(reviewed), false);
      assert.equal(f.writes().length, 0); assert.equal(f.manager.state.result.status, 'not_sent');
    }
    const f = await fixture(kind), reviewed = await f.review('SQ:8');
    f.lists.get('Den@S1/SQ:8').revision += ':changed';
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(`${kind}: a source changing between selected occurrences cannot produce a mixed review`, async () => {
    const f = await fixture(kind); let reads = 0;
    f.read((url, read) => {
      const value = read(url);
      if (url.includes('/queue?') && ++reads === 1) f.queues.get('Den@S1').revision += ':changed';
      return value;
    });
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 0);
    assert.match(f.manager.state.message, /source tracks changed/i);
  });

  test(`${kind}: unavailable occurrences and a destination exceeding 1000 tracks are refused before sending`, async () => {
    const f = await fixture(kind); delete f.queues.get('Den@S1').rows[1].uri;
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 0);
    const g = await fixture(kind); g.lists.get('Den@S1/SQ:7').rows = Array.from({ length: 999 }, () => ({ title: 'Existing', uri: 'fixture:existing' }));
    assert.equal(await g.review(), null); assert.equal(g.writes().length, 0);
    assert.match(g.manager.state.message, /at most 1,000/);
  });

  test(`${kind}: editing an addition or changing households consumes its review`, async () => {
    for (const change of [f => f.manager.editAppend('sourcePositions', '1'), f => f.manager.editAppend('sourcePlaylist', 'SQ:8'),
      f => f.otherHousehold(), f => f.api.catalogChanged({ household: 'S1', playlists: true, favorites: false })]) {
      const f = await fixture(kind), reviewed = await f.review(); await change(f);
      assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: closing during source inspection cannot revive a review or send an addition`, async () => {
    const f = await fixture(kind), gate = deferred(); let waiting = false;
    f.read(async (url, read) => { const value = read(url); if (url.includes('/queue?')) { waiting = true; await gate.promise; } return value; });
    const work = f.review(); await until(() => waiting); f.api.close(); gate.resolve();
    assert.equal(await work, null); await f.reopen();
    assert.equal(f.manager.state.pending, null); assert.equal(f.writes().length, 0);
  });

  test(`${kind}: a partial addition recovers original saved contents after the selected household moves`, async () => {
    const f = await fixture(kind), reviewed = await f.review();
    f.write((url, body, write) => { write(url, body, 2); throw new Error('Stopped with 2 of 3 additions confirmed'); });
    assert.equal(await f.manager.confirm(reviewed), false);
    assert.equal(f.writes().length, 1); assert.ok(f.manager.state.recovery);
    assert.equal(await f.manager.review('append_tracks'), null);
    await f.otherHousehold();
    const start = f.calls.length;
    assert.ok(await f.manager.recover());
    assert.equal(f.manager.state.recovery, null);
    assert.match(f.manager.state.result.detail, /2 of 3 additions confirmed/);
    assert.match(f.manager.state.result.detail, /5 tracks/);
    assert.match(f.manager.state.result.detail, /4: Queue second; 5: Queue first/);
    assert.equal(f.manager.state.result.group.target, 'Den@S1');
    const contents = f.calls.slice(start).filter(call => call.url.includes('/contents?'));
    assert.equal(contents.length, 2);
    assert.ok(contents.every(call => new URL(call.url, 'http://localhost').searchParams.get('zone') === 'Den@S1'));
    assert.equal(f.writes().length, 1);
    if (kind === 'dashboard') assert.equal(f.controller.state.uncertain, false);
  });

  test(`${kind}: unreadable original contents cannot acknowledge an uncertain append`, async () => {
    const f = await fixture(kind);
    f.write((url, body, write) => { write(url, body, 1); throw new Error('Lost remaining reply'); });
    await f.manager.confirm(await f.review());
    f.read((url, read) => { if (url.includes('/contents?')) throw new Error('Original household offline'); return read(url); });
    assert.equal(await f.manager.recover(), null); assert.ok(f.manager.state.recovery);
    if (kind === 'dashboard') assert.equal(f.controller.state.uncertain, true);
    assert.equal(f.writes().length, 1);
  });

  test(`${kind}: contradictory receipts never claim a successful batch or replay it`, async () => {
    for (const patch of [{ added: 2 }, { total: 20 }, { positions: [1, 2, 1] }, { titles: ['wrong'] },
      { revision: 'S1:SQ:7:1' }, { playlist: { id: 'SQ:8', title: 'Destination <saved>' } }, { logged: null }]) {
      const f = await fixture(kind), reviewed = await f.review();
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      assert.equal(await f.manager.confirm(reviewed), false); assert.ok(f.manager.state.recovery);
      assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 1);
    }
  });

  test(`${kind}: one in-flight append retains its late confirmed result after close`, async () => {
    const f = await fixture(kind), reviewed = await f.review(), gate = deferred();
    f.write(async (url, body, write) => { const result = write(url, body); await gate.promise; return { ...result, logged: false }; });
    const work = f.manager.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.manager.editAppend('sourcePositions', '1'), false);
    f.api.close(); gate.resolve(); assert.equal(await work, true);
    assert.equal(f.manager.state.result.status, 'accepted'); assert.equal(f.manager.state.recovery, null);
    assert.match(f.manager.state.result.detail, /do not repeat/i); assert.equal(f.writes().length, 1);
  });
}

test('append input rejects ambiguous or unbounded positions before any reads', async () => {
  const f = await fixture('phone');
  for (const positions of ['', '0', '1,', ',2', '1.5', '1e2', '1 2', '100001', '1,'.repeat(10) + '1']) {
    f.manager.editAppend('sourcePositions', positions);
    assert.equal(await f.manager.review('append_tracks'), null);
  }
  assert.equal(f.calls.length, 0);
  assert.deepEqual(Phone.playlistAppendSelection('', ' 2, 1,2 ', []).positions, [2, 1, 2]);
});

test('both remotes share addition controls that show titles, order and destination as text', async () => {
  const f = await fixture('phone');
  const doc = { createElement(tag) {
    const events = new Map();
    return { tag, children: [], value: '', textContent: '', disabled: false, attributes: {},
      append(...nodes) { this.children.push(...nodes); }, replaceChildren(...nodes) { this.children = nodes; },
      setAttribute(name, value) { this.attributes[name] = value; },
      addEventListener(name, fn) { events.set(name, fn); }, fire(name) { return events.get(name)?.(); },
    };
  } };
  const panel = Phone.playlistManagerPanel(f.manager, doc); panel.render(true);
  const all = node => [node, ...node.children.flatMap(all)];
  const source = all(panel.node).find(node => node.attributes['aria-label'] === 'Playlist addition source');
  const positions = all(panel.node).find(node => node.attributes['aria-label'] === 'Track numbers to add');
  source.value = 'SQ:8'; source.fire('change'); positions.value = '2,1,2'; positions.fire('input');
  all(panel.node).find(node => node.textContent === 'Review additions').fire('click');
  await until(() => f.manager.state.pending); panel.render(true);
  const text = all(panel.node).map(node => node.textContent).join('\n');
  assert.match(text, /Other & source/); assert.match(text, /Destination <saved>/);
  assert.match(text, /Source track 2: Second track/); assert.match(text, /Source track 1: First <track>/);
  assert.match(text, /6 tracks in total/); assert.equal(f.writes().length, 0);
  assert.ok(all(panel.node).every(node => !Object.hasOwn(node, 'innerHTML')));
  all(panel.node).find(node => node.textContent === 'Add selected tracks to playlist').fire('click');
  await until(() => f.manager.state.result); assert.equal(f.writes().length, 1);
});
