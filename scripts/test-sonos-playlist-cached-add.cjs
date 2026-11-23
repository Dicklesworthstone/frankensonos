// Both production remotes add an observed cached basket to native saved music
// without first loading that music into a speaker's playing queue.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Library = require('../crates/fsonos-api/src/remote/library.js');
const DashboardLibrary = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const Q = require('../crates/fsonos-api/src/remote/queue-tracks.js');
const copy = value => structuredClone(value);
const uri = n => `spotify:track:${String(n).padStart(22, '0')}`;
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) { for (let i = 0; i < 300 && !check(); ++i) await Promise.resolve(); assert.ok(check()); }

async function fixture(kind, count = 103) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' }));
  const catalog = [{ id: 'SQ:7', title: 'Destination <saved>' }, { id: 'SQ:8', title: 'Other & saved' }];
  const lists = new Map();
  for (const zone of zones) for (const item of catalog) lists.set('Den@' + zone.household + '/' + item.id, {
    revision: zone.household + ':' + item.id + ':1', rows: Array.from({ length: count }, (_, n) => ({
      title: `Saved ${n + 1}`, uri: `x-file-cifs://example.invalid/${n + 1}.flac`, creator: 'Fixture artist',
    })),
  });
  const calls = [], state = { libraryRevision: 'library:1', verifies: 0, lane: 0 };
  let reader, writer, api, controller;
  function read(url) {
    const parsed = new URL(url, 'http://localhost'), query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/schedules') return [];
    if (parsed.pathname === '/sonos/playlists') return copy(catalog);
    if (parsed.pathname === '/sonos/playlists/contents') {
      const list = lists.get(query.zone + '/' + query.playlist), item = catalog.find(item => item.id === query.playlist);
      if (!list || !item) throw new Error('Saved playlist unavailable.');
      if (query.expected_revision && query.expected_revision !== list.revision) throw new Error('Saved playlist changed.');
      const offset = Number(query.offset || 0), limit = Number(query.limit || 100);
      const items = list.rows.slice(offset, offset + limit).map((track, i) => ({ ...copy(track), position: offset + i + 1 }));
      return { zone: 'Den', playlist: copy(item), revision: list.revision, total: list.rows.length, offset,
        next_offset: offset + items.length < list.rows.length ? offset + items.length : null, items };
    }
    if (parsed.pathname === '/library/browse') {
      ++state.verifies;
      const offset = Number(query.offset || 0), total = 53, items = Array.from({ length: Math.min(50, total - offset) }, (_, i) => ({
        kind: 'track', source_uri: uri(offset + i + 1), title: `Cached <${offset + i + 1}>`, subtitle: 'Cached artist', explicit: false,
      }));
      return { kind: 'tracks', offset, limit: 50, total, album_uri: null, coverage: 'cached', revision: state.libraryRevision,
        next_offset: offset + items.length < total ? offset + items.length : null, items };
    }
    if (parsed.pathname.endsWith('/state')) {
      const target = decodeURIComponent(parsed.pathname.split('/')[2]);
      return { zone: copy(zones.find(zone => 'Den@' + zone.household === target)), transport_state: 'paused', volume: 20,
        mute: false, track: { uri: 'fixture:owner-playing', title: 'Current music' } };
    }
    throw new Error('Unexpected read: ' + url);
  }
  function write(url, body, count = body.tracks.length) {
    assert.equal(url, '/sonos/playlists/add-tracks', 'only the saved playlist is written');
    const list = lists.get(body.zone + '/' + body.playlist), previous_total = list.rows.length;
    assert.equal(body.expected_revision, list.revision);
    list.rows.push(...body.tracks.slice(0, count).map(track => ({ title: track.title || track.source_uri, uri: track.source_uri })));
    list.revision += ':next';
    return { done: 'Added selected cached tracks.', zone: 'Den', playlist: copy(catalog.find(item => item.id === body.playlist)),
      revision: list.revision, previous_total, total: list.rows.length, added: count,
      source_uris: body.tracks.map(track => track.source_uri), titles: body.tracks.map(track => track.title || track.source_uri), logged: true };
  }
  const call = async (url, body) => {
    calls.push({ url, body: copy(body) });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  const routed = async (name, args = {}) => {
    const route = Phone.routes[name] || Library.routes[name] || { zones: ['GET', '/zones'], state: ['GET', '/zones/{room}/state'] }[name];
    assert.ok(route, 'known endpoint ' + name);
    const path = route[1].replace('{room}', encodeURIComponent(args.params?.room));
    return call(path + (args.query ? '?' + new URLSearchParams(args.query) : ''), route[0] === 'POST' ? args.body : undefined);
  };
  if (kind === 'phone') api = Phone.model({ call: routed, execute: async (_, work) => { ++state.lane; return work(); } });
  else {
    controller = Core.model({ call, changed: () => api?.observe() });
    api = Dashboard.model({ call, controller });
    assert.equal(await controller.refresh(), true); controller.connection(true);
  }
  const library = Library.model({ call: routed });
  assert.equal(await library.refreshRooms(), true); library.target('Den@S1');
  assert.equal(await library.browse('tracks'), true); library.selectTrack(1, true);
  assert.equal(await library.next(), true); library.selectTrack(2, true);
  library.repeatTrack(library.state.basket[0].id);
  let selection;
  if (kind === 'phone') selection = await library.playlistSelection();
  else {
    // The dashboard panel uses this same exported verifier and immutable choice.
    const pages = [0, 50].map(offset => DashboardLibrary.pageOf(read('/library/browse?' + new URLSearchParams({ kind: 'tracks', offset })), { kind: 'tracks', offset }));
    const choice = DashboardLibrary.selectionOf({ rows: [
      { page: pages[0], index: 1, item: pages[0].items[1] },
      { page: pages[0], index: 1, item: pages[0].items[1] },
      { page: pages[1], index: 2, item: pages[1].items[2] },
    ] });
    const tracks = Q.tracksOf(choice.rows.map(row => row.item));
    selection = { group: Phone.groups(zones)[0], tracks, verify: async check => {
      await DashboardLibrary.verifySelection(call, choice, check); check(); return tracks;
    } };
  }
  assert.ok(selection); assert.equal(await api.offerTracks(selection), true);
  library.close(); // Handoff owns the frozen proof after its chooser closes.
  assert.equal(api.edit('playlist', 'SQ:7'), true); calls.length = 0; state.verifies = 0;
  return { api, controller, manager: api.manager, calls, lists, state, selection, zones, routed,
    read(fn) { reader = fn; }, write(fn) { writer = fn; },
    writes: () => calls.filter(call => call.body !== undefined),
    review: () => api.manager.review('add_tracks'),
  };
}

for (const kind of ['phone', 'dashboard']) {
  test(`${kind}: cached multi-page basket appends in order with repeats without touching playback`, async () => {
    const f = await fixture(kind), other = copy(f.lists.get('Den@S2/SQ:7'));
    assert.equal(await f.api.review(), kind === 'phone' ? false : null, 'cached addition cannot become playback');
    const reviewed = await f.review(); assert.ok(reviewed);
    assert.equal(reviewed.destinationTracks.length, 103, 'complete destination included across short final page');
    assert.deepEqual(reviewed.cached.tracks.map(track => track.source_uri), [uri(2), uri(2), uri(53)]);
    assert.equal(f.state.verifies, 2, 'each selected source page rechecked once');
    assert.equal(f.writes().length, 0);
    assert.equal(await f.manager.confirm(reviewed), true);
    assert.equal(f.state.verifies, 4, 'every selected source page rechecked on confirmation');
    assert.equal(f.writes().length, 1); assert.equal(await f.manager.confirm(reviewed), false);
    assert.deepEqual(f.lists.get('Den@S2/SQ:7'), other, 'same SQ id in other household unchanged');
    assert.deepEqual(f.lists.get('Den@S1/SQ:7').rows.slice(-3).map(track => track.uri), [uri(2), uri(2), uri(53)]);
    assert.equal(f.manager.state.cached, null, 'dispatched selection is consumed');
    assert.equal(f.manager.state.result.status, 'accepted');
    assert.ok(f.calls.every(call => !/\/queue|\/play(?:\/|$)/.test(call.url)), 'no playing queue read or write needed');
    if (kind === 'phone') assert.equal(f.state.lane, 1);
  });

  test(`${kind}: empty destination accepts exact cached selection`, async () => {
    const f = await fixture(kind, 0), reviewed = await f.review();
    assert.ok(reviewed); assert.deepEqual(reviewed.destinationTracks, []);
    assert.equal(await f.manager.confirm(reviewed), true); assert.equal(f.lists.get('Den@S1/SQ:7').rows.length, 3);
  });

  test(`${kind}: source-page revision change retires confirmation before any write`, async () => {
    const f = await fixture(kind), reviewed = await f.review();
    f.state.libraryRevision = 'library:changed';
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    assert.equal(f.manager.state.result.status, 'not_sent'); assert.equal(f.manager.state.pending, null);
  });

  test(`${kind}: destination revision or contents drift cannot reuse a review`, async () => {
    for (const revision of [false, true]) {
      const f = await fixture(kind), reviewed = await f.review(), list = f.lists.get('Den@S1/SQ:7');
      if (revision) list.revision += ':other'; else list.rows[102].title = 'Externally changed last occurrence';
      assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    }
  });

  test(`${kind}: concurrent confirmations use one writer and one request`, async () => {
    const f = await fixture(kind), reviewed = await f.review(), wait = deferred();
    f.write(async (url, body, write) => { await wait.promise; return write(url, body); });
    const first = f.manager.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(await f.manager.confirm(reviewed), false); wait.resolve(); assert.equal(await first, true);
    assert.equal(f.writes().length, 1);
  });

  test(`${kind}: partial addition stays uncertain until original contents are explicitly observed`, async () => {
    const f = await fixture(kind), reviewed = await f.review();
    f.write((url, body, write) => { write(url, body, 1); throw new Error('Connection ended after one addition.'); });
    assert.equal(await f.manager.confirm(reviewed), false);
    assert.equal(f.manager.state.result.status, 'uncertain'); assert.ok(f.manager.state.recovery);
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 1);
    f.api.close();
    if (kind === 'phone') await f.api.refresh(); else { f.controller.select('Den@S2'); f.api.open(); }
    f.calls.length = 0;
    const observed = await f.manager.recover(); assert.ok(observed);
    assert.equal(f.manager.state.result.status, 'observed'); assert.equal(f.manager.state.recovery, null);
    assert.ok(f.calls.filter(call => call.url.startsWith('/sonos/')).every(call => new URL(call.url, 'http://localhost').searchParams.get('zone') === 'Den@S1'));
    assert.ok(f.calls.some(call => call.url.includes('/contents?') && call.url.includes('offset=103')));
    assert.equal(f.writes().length, 0, 'recovery never replays the addition');
    assert.match(f.manager.state.result.detail, /does not prove how the earlier request finished/);
  });

  test(`${kind}: mismatched receipt and lost logging have distinct outcomes`, async () => {
    const bad = await fixture(kind), reviewed = await bad.review();
    bad.write((url, body, write) => ({ ...write(url, body), source_uris: [uri(9)] }));
    assert.equal(await bad.manager.confirm(reviewed), false); assert.ok(bad.manager.state.recovery);
    const ok = await fixture(kind), second = await ok.review();
    ok.write((url, body, write) => ({ ...write(url, body), logged: false }));
    assert.equal(await ok.manager.confirm(second), true); assert.equal(ok.manager.state.recovery, null);
    assert.match(ok.manager.state.result.detail, /action log could not be written/);
  });

  test(`${kind}: destination capacity rejects the addition before dispatch`, async () => {
    const f = await fixture(kind, 999);
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 0);
    assert.match(f.manager.state.message, /1,000 tracks/);
  });

  test(`${kind}: a closed or changed destination consumes the pending review`, async () => {
    const f = await fixture(kind), reviewed = await f.review();
    assert.equal(f.api.edit('playlist', 'SQ:8'), true);
    assert.equal(await f.manager.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    const next = await f.review(); assert.ok(next);
    f.api.close(); assert.equal(await f.manager.confirm(next), false); assert.equal(f.writes().length, 0);
  });
}

test('cached handoff accepts track and episode occurrences, normalizes titles, and rejects containers', () => {
  const group = Phone.groups([{ household: 'S1', coordinator_room: 'Den', members: ['Den'] }])[0];
  const episode = `spotify:episode:${'x'.repeat(22)}`;
  const selection = Phone.cachedPlaylistSelection({ group, tracks: [
    { source_uri: episode, title: '\u0085Episode\u0085' }, { source_uri: uri(1), title: ' ' },
  ], verify: async () => [] });
  assert.deepEqual(selection.tracks, [{ source_uri: episode, title: 'Episode' }, { source_uri: uri(1) }]);
  assert.ok(Object.isFrozen(selection.tracks));
  assert.throws(() => Phone.cachedPlaylistSelection({ group, tracks: [{ source_uri: `spotify:album:${'x'.repeat(22)}` }], verify() {} }), /unsupported/);
});

for (const changed of [false, true]) test(`phone: deferred destination handoff ${changed ? 'preserves a newer basket' : 'locks the offered basket until accepted'}`, async () => {
  const f = await fixture('phone'), wait = deferred(); f.api.clearDraft();
  const picker = Library.model({ call: f.routed });
  assert.equal(await picker.refreshRooms(), true); picker.target('Den@S1');
  assert.equal(await picker.browse('tracks'), true); picker.selectTrack(1, true);
  const original = picker.state.basket, id = original[0].id;
  let destinationReading = false;
  f.read(async (url, read) => {
    if (url.startsWith('/sonos/playlists?')) { destinationReading = true; await wait.promise; }
    return read(url);
  });
  const handoff = picker.playlistSelection(selection => f.api.offerTracks(selection));
  await until(() => destinationReading);
  assert.equal(picker.state.loading, true);
  assert.equal(picker.repeatTrack(id), false); assert.equal(picker.selectTrack(2, true), false);
  picker.clearSelection(); assert.equal(picker.state.basket, original);
  assert.equal(await picker.chooseTracks('play'), false);
  if (changed) {
    picker.target('Den@S2'); assert.equal(picker.repeatTrack(id), true);
    assert.equal(picker.state.basket.length, 2);
  }
  wait.resolve(); assert.equal(await handoff, !changed);
  assert.equal(picker.state.basket.length, changed ? 2 : 0);
  assert.equal(picker.state.loading, false); assert.equal(f.manager.state.cached.tracks.length, 1);
  assert.equal(f.manager.state.cached.group.target, 'Den@S1');
  assert.equal(f.writes().length, 0, 'handoff never writes or starts playback');
});

test('phone: rejected source handoff retains the selected basket', async () => {
  const f = await fixture('phone'), picker = Library.model({ call: f.routed });
  await picker.refreshRooms(); picker.target('Den@S1'); await picker.browse('tracks'); picker.selectTrack(1, true);
  const original = picker.state.basket;
  assert.equal(await picker.playlistSelection(selection => f.api.offerTracks(selection)), false, 'existing source draft refuses replacement');
  assert.equal(picker.state.basket, original); assert.equal(picker.state.loading, false);
  assert.match(picker.state.message, /Finish or clear/); assert.equal(f.writes().length, 0);
});

for (const kind of ['phone', 'dashboard']) test(`${kind}: shared addition component renders all ordered occurrences and native contents as text`, async () => {
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.events = new Map(); this.attributes = {}; this.value = ''; this.textContent = ''; }
    append(...nodes) { this.children.push(...nodes); }
    replaceChildren(...nodes) { this.children = nodes; }
    setAttribute(key, value) { this.attributes[key] = value; }
    addEventListener(key, fn) { this.events.set(key, fn); }
    set innerHTML(_) { assert.fail('Cached and native metadata must render as text'); }
  }
  const f = await fixture(kind), descendants = node => [node, ...node.children.flatMap(descendants)];
  f.lists.get('Den@S1/SQ:7').rows[102].title = '<script>Last & saved</script>';
  const panel = Phone.playlistManagerPanel(f.manager, { createElement: tag => new Element(tag) });
  const find = label => descendants(panel.node).find(node => node.textContent === label);
  panel.render(true);
  assert.equal(panel.node.children[0].hidden, true, 'other catalog edits stay hidden during this draft');
  assert.equal(find('Review selected additions').disabled, false);
  find('Review selected additions').events.get('click')();
  await until(() => !!f.manager.state.pending); panel.render(true);
  const nodes = descendants(panel.node), text = nodes.map(node => node.textContent).join(' ');
  assert.match(text, /Destination <saved>.*SQ:7.*S1/);
  assert.match(text, /103 tracks; 106 in total/);
  assert.match(text, /Repeated selections add repeated copies/);
  assert.match(text, /Current playback and queues stay unchanged/);
  const ordered = nodes.find(node => node.attributes['aria-label'] === 'Reviewed additions in order');
  assert.deepEqual(ordered.children.map(node => node.textContent), [
    `Cached <2> · ${uri(2)}`, `Cached <2> · ${uri(2)}`, `Cached <53> · ${uri(53)}`,
  ]);
  const contents = nodes.find(node => node.tag === 'details');
  assert.equal(contents.children[0].textContent, 'Existing saved contents: 103 tracks');
  assert.equal(contents.children[1].children.length, 103);
  assert.equal(contents.children[1].children[102].textContent, '<script>Last & saved</script> · Fixture artist');
  assert.equal(f.writes().length, 0);
  const confirm = find('Add selected tracks to playlist'); confirm.events.get('click')(); confirm.events.get('click')();
  await until(() => f.manager.state.result?.status === 'accepted'); panel.render(true);
  assert.equal(f.writes().length, 1); assert.equal(panel.node.children[1].hidden, true, 'accepted draft is consumed');
});
