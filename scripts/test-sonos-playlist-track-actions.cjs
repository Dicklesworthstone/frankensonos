// Exercise the production phone and dashboard command models against a native
// catalog, including changes between inspection, review and dispatch.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(predicate) {
  for (let i = 0; i < 200 && !predicate(); ++i) await Promise.resolve();
  assert.ok(predicate(), 'reached the asynchronous command boundary');
}

async function fixture(kind) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'], transport_state: 'paused' }));
  const catalog = [{ id: 'SQ:7', title: 'Saved & repeated' }, { id: 'SQ:8', title: 'Other list' }];
  const entries = [
    { title: 'First occurrence', uri: 'x-file-cifs://example.invalid/repeated.flac', creator: 'Artist' },
    { title: 'Second occurrence', uri: 'x-file-cifs://example.invalid/repeated.flac', creator: 'Artist' },
    { title: 'Unavailable' },
  ];
  const rows = new Map(zones.map(zone => ['Den@' + zone.household, { zone, transport_state: 'paused', volume: 12, mute: false,
    track: { uri: 'fixture:playing', title: 'Owner song', queue_position: 1, position_secs: 10, duration_secs: 300 } }]));
  const queues = new Map(zones.map(zone => ['Den@' + zone.household, { zone: 'Den', total: 2, revision: 'queue:' + zone.household,
    playing: 1, items: [{ position: 1, uri: 'fixture:playing', title: 'Owner song' }] }]));
  let revision = 'native:1', reader = null, writer = null, api, controller;
  const calls = [];
  const read = url => {
    const parsed = new URL(url, 'http://localhost');
    const query = Object.fromEntries(parsed.searchParams);
    if (parsed.pathname === '/zones') return copy(zones);
    if (parsed.pathname === '/schedules') return [];
    if (parsed.pathname === '/sonos/playlists') return copy(catalog);
    if (parsed.pathname === '/sonos/playlists/contents') {
      if (query.expected_revision && query.expected_revision !== revision) throw new Error('The saved playlist changed. Read tracks again.');
      const offset = Number(query.offset), limit = Number(query.limit);
      const items = entries.slice(offset, offset + limit).map((item, index) => ({ ...item, position: offset + index + 1 }));
      return { zone: 'Den', playlist: copy(catalog.find(item => item.id === query.playlist)), revision, total: entries.length,
        offset, next_offset: offset + items.length < entries.length ? offset + items.length : null, items };
    }
    const target = decodeURIComponent(parsed.pathname.split('/')[2] || '');
    if (parsed.pathname.endsWith('/state')) return copy(rows.get(target));
    if (parsed.pathname.endsWith('/queue')) return copy(queues.get(target));
    throw new Error('Unexpected read ' + url);
  };
  const write = (url, body) => {
    assert.equal(url, '/sonos/playlists/track', 'never fall back to a whole playlist or raw URI');
    const q = queues.get(body.zone);
    return { done: body.mode === 'play' ? 'Selected track playing' : 'One selected track queued', changed: true, notes: [{ detail: 'Policy applied.' }],
      zone: 'Den', playlist: body.playlist, position: body.position, title: entries[body.position - 1].title, mode: body.mode,
      ...(body.mode === 'play' ? {} : { first: body.mode === 'next' ? q.playing + 1 : q.total + 1, count: 1, playing_queue: true }) };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  if (kind === 'phone') {
    api = Phone.model({ call(name, args = {}) {
      const [method, path] = Phone.routes[name] || (name === 'zones' ? ['GET', '/zones'] : ['GET', '/zones/{room}/state']);
      const url = path.replace('{room}', encodeURIComponent(args.params?.room)) + (args.query ? '?' + new URLSearchParams(args.query) : '');
      return call(url, method === 'POST' ? args.body : undefined);
    } });
    await api.refresh(); api.edit('target', 'Den@S1');
  } else {
    controller = Core.model({ call, changed: () => api?.observe() });
    api = Dashboard.model({ call, controller });
    assert.equal(await controller.refresh(), true); controller.connection(true); api.open();
  }
  api.edit('sourceType', 'playlist'); await api.loadPlaylists(); api.edit('playlist', 'SQ:7');
  assert.equal(await api.loadContents(), true);
  return { api, controller, calls, entries, rows, queues, catalog,
    writes: () => calls.filter(call => call.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, revision(value) { revision = value; },
    async review(position = 2, mode = 'play', page) { await api.reviewTrack(position, mode, page); return api.state.pending; },
    confirm(pending = api.state.pending) { return kind === 'phone' ? api.confirm(pending) : api.apply(pending); },
    selectOtherHousehold() { if (kind === 'phone') api.edit('target', 'Den@S2'); else { controller.select('Den@S2'); api.observe(); } },
  };
}

for (const kind of ['phone', 'dashboard']) {
  test(kind + ': native playlist changes retire reviewed occurrences only in their own household', async () => {
    const f = await fixture(kind), reviewed = await f.review(), page = f.api.state.contents;
    f.api.catalogChanged({ household: 'S2', favorites: false, playlists: true });
    assert.equal(f.api.state.pending, reviewed); assert.equal(f.api.state.contents, page);
    f.api.catalogChanged({ household: 'S1', favorites: true, playlists: false });
    assert.equal(f.api.state.pending, reviewed);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    assert.equal(f.api.state.pending, null); assert.equal(f.api.state.contents, null);
    assert.equal(f.api.state.playlist, ''); assert.deepEqual(f.api.state.playlists, []);
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a catalog event fences an outstanding saved-track read', async () => {
    const f = await fixture(kind), held = deferred(); let waiting = false;
    f.read(async (url, read) => {
      const value = read(url);
      if (url.includes('/contents?')) { waiting = true; await held.promise; }
      return value;
    });
    const task = f.api.loadContents(); await until(() => waiting);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    held.resolve(); assert.equal(await task, false);
    assert.equal(f.api.state.contents, null); assert.equal(f.api.state.pending, null); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a catalog event during a submitted native track action preserves its one outcome', async () => {
    const f = await fixture(kind), reviewed = await f.review(), held = deferred();
    f.write(async (url, body, write) => { await held.promise; return write(url, body); });
    const task = f.confirm(reviewed); await until(() => f.writes().length === 1);
    f.api.catalogChanged({ household: 'S1', favorites: false, playlists: true });
    assert.equal(f.api.state.contents, null); assert.equal(f.api.state.pending, null);
    held.resolve(); assert.equal(await task, true); assert.equal(f.api.state.result.status, 'accepted');
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 1);
  });

  for (const mode of ['play', 'next', 'end']) {
    test(`${kind}: ${mode} acts once on the second duplicate occurrence with native revision`, async () => {
      const f = await fixture(kind), reviewed = await f.review(2, mode);
      assert.ok(reviewed); assert.equal(f.writes().length, 0);
      assert.equal(reviewed.item.container, false); assert.equal(reviewed.item.position, 2);
      assert.ok(Object.isFrozen(reviewed.item)); assert.ok(Object.isFrozen(reviewed.body));
      assert.equal(await f.confirm(reviewed), true); assert.equal(await f.confirm(reviewed), false);
      assert.deepEqual(f.writes().map(({ url, body }) => ({ url, body })), [{ url: '/sonos/playlists/track', body: {
        zone: 'Den@S1', playlist: 'SQ:7', position: 2, expected_revision: 'native:1', mode: mode === 'end' ? 'append' : mode,
      } }]);
      assert.match(f.api.state.result.detail, /Policy applied/);
      const exact = f.calls.filter(call => call.url.includes('/contents?') && new URL(call.url, 'http://localhost').searchParams.has('expected_revision'));
      assert.equal(exact.length, 2, 'recheck the exact saved occurrence for review and dispatch');
      for (const call of exact) {
        const query = new URL(call.url, 'http://localhost').searchParams;
        assert.equal(query.get('offset'), '1'); assert.equal(query.get('limit'), '1'); assert.equal(query.get('expected_revision'), 'native:1');
      }
      assert.equal(f.rows.get('Den@S1').transport_state, 'paused', 'the browser never invents a playback state');
    });
  }

  test(kind + ': an unavailable or unobserved occurrence cannot create a review or request', async () => {
    const f = await fixture(kind), count = f.calls.length;
    for (const position of [0, 3, 4, -1, 1.5, 100001]) {
      assert.equal(await f.review(position), null); assert.equal(f.calls.length, count);
    }
    assert.equal(f.writes().length, 0);
  });

  test(kind + ': changed revision invalidates selection both before review and before dispatch', async () => {
    const f = await fixture(kind); f.revision('native:2');
    assert.equal(await f.review(), null); assert.equal(f.writes().length, 0);
    await f.api.loadContents(); const reviewed = await f.review(); assert.ok(reviewed);
    f.revision('native:3'); assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.writes().length, 0); assert.equal(f.api.state.result?.status === 'uncertain', false);
  });

  test(kind + ': replacing one same-position resource or title cannot silently substitute music', async () => {
    for (const field of ['uri', 'title']) {
      const f = await fixture(kind), reviewed = await f.review();
      f.entries[1][field] = field === 'uri' ? 'x-file-cifs://example.invalid/other.flac' : 'Different occurrence';
      assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
    }
  });

  test(kind + ': an old row handler cannot select an occurrence from a refreshed page', async () => {
    const f = await fixture(kind), old = f.api.state.contents;
    await f.api.loadContents(); const before = f.calls.length;
    assert.equal(await f.review(2, 'next', old), null); assert.equal(f.calls.length, before);
    assert.ok(await f.review(2, 'next', f.api.state.contents));
  });

  test(kind + ': a changed household retires the reviewed native ID', async () => {
    const f = await fixture(kind), reviewed = await f.review(); f.selectOtherHousehold();
    assert.equal(f.api.state.pending, null); assert.equal(f.api.state.contents, null);
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': a changed destination queue blocks insertion before sending', async () => {
    const f = await fixture(kind), reviewed = await f.review(2, 'next');
    f.queues.get('Den@S1').revision += ':changed';
    assert.equal(await f.confirm(reviewed), false); assert.equal(f.writes().length, 0);
  });

  test(kind + ': late saved-track reads cannot revive closed or edited selections', async () => {
    for (const change of ['close', 'playlist']) {
      const f = await fixture(kind), gate = deferred(); let waiting = false;
      f.read(async (url, read) => {
        const value = read(url);
        if (url.includes('/contents?')) { waiting = true; await gate.promise; }
        return value;
      });
      const task = f.review(); await until(() => waiting);
      if (change === 'close') f.api.close(); else f.api.edit('playlist', 'SQ:8');
      gate.resolve(); assert.equal(await task, null); assert.equal(f.api.state.pending, null); assert.equal(f.writes().length, 0);
    }
  });

  test(kind + ': lost replies are uncertain and the same reviewed track is never replayed', async () => {
    const f = await fixture(kind), reviewed = await f.review(2, 'end');
    f.write(() => { throw new Error('The reply was lost'); });
    assert.equal(await f.confirm(reviewed), false); assert.equal(await f.confirm(reviewed), false);
    assert.equal(f.api.state.result.status, 'uncertain'); assert.equal(f.writes().length, 1);
  });

  test(kind + ': mismatched receipts do not claim a selected-track success', async () => {
    for (const patch of [{ playlist: 'SQ:8' }, { position: 1 }, { count: 3 }, { mode: 'play' }, { title: 'Other' }, { zone: 'Other' }]) {
      const f = await fixture(kind), reviewed = await f.review(2, 'next');
      f.write((url, body, write) => ({ ...write(url, body), ...patch }));
      assert.equal(await f.confirm(reviewed), false); assert.equal(f.api.state.result.status, 'uncertain');
      assert.equal(f.writes().length, 1);
    }
  });

  test(kind + ': repeated clicks and close/reopen cannot duplicate an in-flight track action', async () => {
    const f = await fixture(kind), reviewed = await f.review(), gate = deferred();
    f.write(async (url, body, write) => { await gate.promise; return write(url, body); });
    const writing = f.confirm(reviewed); await until(() => f.writes().length === 1);
    assert.equal(await f.confirm(reviewed), false); f.api.close();
    if (kind === 'phone') await f.api.refresh(); else f.api.open();
    assert.equal(await f.confirm(reviewed), false); gate.resolve();
    assert.equal(await writing, true); assert.equal(f.writes().length, 1); assert.equal(f.api.state.result.status, 'accepted');
  });
}

test('row actions retain exact occurrence and render track metadata as text', async () => {
  const doc = { createElement(tag) { const handlers = new Map(); return { tag, children: [], textContent: '', disabled: false,
    append(node) { this.children.push(node); }, setAttribute(name, value) { this[name] = value; },
    addEventListener(name, handler) { handlers.set(name, handler); }, click() { handlers.get('click')?.(); } }; } };
  const list = { children: [], replaceChildren() { this.children = []; }, append(node) { this.children.push(node); } };
  const page = { items: [{ position: 52, title: '<img onerror=alert(1)>', uri: 'fixture:track' }, { position: 53, title: 'Unavailable' }] };
  const actions = [];
  Phone.renderContents(list, page, doc, { review: (...args) => actions.push(args) });
  assert.equal(list.children[0].children[0].textContent, '<img onerror=alert(1)>');
  const buttons = list.children[0].children.at(-1).children;
  assert.deepEqual(buttons.map(button => button.textContent), ['Play now', 'Play next', 'Add to queue']);
  buttons[1].click(); assert.deepEqual(actions, [[52, 'next', page]]);
  assert.ok(list.children[1].children.at(-1).children.every(button => button.disabled));
  Phone.renderContents(list, page, doc, { review: (...args) => actions.push(args), disabled: true });
  list.children[0].children.at(-1).children[0].click(); assert.equal(actions.length, 1);
});
