// The shipped phone and dashboard readers against changing saved catalogs.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Phone = require('../crates/fsonos-api/src/remote/source-player.js');
const Dashboard = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const clone = value => structuredClone(value);
const defer = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };

async function fixture(kind) {
  const zones = ['S1', 'S2'].map(household => ({ household, coordinator_room: 'Den', members: ['Den'] }));
  const catalog = [{ id: 'SQ:7', title: '<Evening & friends>' }, { id: 'SQ:8', title: 'Another set' }];
  const entries = [
    { title: '<script>untrusted title</script>', creator: 'An artist', album: 'Album', uri: 'fixture:duplicate', duration_secs: 65 },
    { title: '<script>untrusted title</script>', creator: 'An artist', album: 'Album', uri: 'fixture:duplicate', duration_secs: 65 },
    { title: 'Final track', creator: 'Another artist', uri: 'fixture:final' },
  ];
  let revision = 'saved:1', reader = null;
  const calls = [];
  const page = query => {
    if (query.expected_revision && query.expected_revision !== revision) throw new Error('The playlist changed. Read tracks again.');
    const offset = Number(query.offset), items = entries.slice(offset, offset + 2).map((item, i) => ({ ...item, position: offset + i + 1 }));
    return { zone: 'Den', playlist: clone(catalog.find(item => item.id === query.playlist)), revision,
      total: entries.length, offset, next_offset: offset + items.length < entries.length ? offset + items.length : null, items };
  };
  async function read(name, query = {}) {
    calls.push({ name, query: clone(query) });
    if (name === 'zones') return clone(zones);
    if (name === 'catalog') return clone(catalog);
    assert.equal(name, 'contents', 'the reader never sends playback commands or loads track URLs');
    return reader ? reader(query, page) : page(query);
  }
  let api, controller;
  if (kind === 'phone') {
    api = Phone.model({ call(name, args) {
      return read({ zones: 'zones', sourcePlaylists: 'catalog', sourcePlaylistContents: 'contents' }[name] || name, args?.query);
    } });
    await api.refresh(); api.edit('target', 'Den@S1');
  } else {
    controller = {
      state: { selected: 'Den@S1', busy: false, topologyFresh: true, active: true },
      selected() { return { zone: zones.find(zone => 'Den@' + zone.household === this.state.selected), fresh: true }; },
      canControl() { return true; },
    };
    api = Dashboard.model({ controller, call(url, body) {
      assert.equal(body, undefined, 'contents browsing never writes');
      const parsed = new URL(url, 'http://localhost');
      return read(parsed.pathname === '/sonos/playlists' ? 'catalog' : parsed.pathname === '/sonos/playlists/contents' ? 'contents' : parsed.pathname,
        Object.fromEntries(parsed.searchParams));
    } });
    api.open();
  }
  api.edit('sourceType', 'playlist'); await api.loadPlaylists(); api.edit('playlist', 'SQ:7');
  return { api, calls, entries, catalog, reader(fn) { reader = fn; }, revision(value) { revision = value; },
    select() { if (kind === 'phone') api.edit('target', 'Den@S2'); else { controller.state.selected = 'Den@S2'; api.observe(); } },
    contentsCalls() { return calls.filter(call => call.name === 'contents'); } };
}

for (const kind of ['phone', 'dashboard']) {
  test(kind + ': explicit reads preserve duplicate occurrences and page both directions at one revision', async () => {
    const f = await fixture(kind);
    assert.equal(f.contentsCalls().length, 0, 'selecting a playlist does not start background requests');
    assert.equal(await f.api.loadContents(), true);
    assert.deepEqual(f.api.state.contents.items.map(item => item.position), [1, 2]);
    assert.equal(f.api.state.contents.items[0].uri, f.api.state.contents.items[1].uri);
    assert.ok(Object.isFrozen(f.api.state.contents.items));
    assert.equal(f.api.state.pending, null);
    assert.match(Phone.contentsSummary(f.api.state.contents), /Tracks 1–2 of 3/);
    assert.equal(await f.api.loadContents('next'), true);
    assert.deepEqual(f.api.state.contents.items.map(item => item.position), [3]);
    assert.equal(f.api.state.contents.next_offset, null);
    assert.equal(f.contentsCalls()[1].query.expected_revision, 'saved:1');
    assert.equal(Number(f.contentsCalls()[1].query.offset), 2);
    assert.equal(await f.api.loadContents('next'), false);
    assert.equal(await f.api.loadContents('previous'), true);
    assert.deepEqual(f.api.state.contents.items.map(item => item.position), [1, 2]);
    assert.deepEqual(f.api.state.contentsHistory, []);
    assert.equal(f.contentsCalls()[2].query.expected_revision, 'saved:1');
    assert.ok(f.contentsCalls().every(call => call.query.zone === 'Den@S1' && call.query.playlist === 'SQ:7'));
  });

  test(kind + ': changed contents invalidate continuation until an explicit fresh read', async () => {
    const f = await fixture(kind); await f.api.loadContents(); f.revision('saved:2');
    assert.equal(await f.api.loadContents('next'), false);
    assert.equal(f.api.state.contents, null); assert.deepEqual(f.api.state.contentsHistory, []);
    assert.match(f.api.state.message, /playlist changed/);
    assert.equal(await f.api.loadContents('next'), false);
    assert.equal(f.contentsCalls().length, 2);
    assert.equal(await f.api.loadContents(), true);
    assert.equal(f.api.state.contents.revision, 'saved:2');
    assert.equal(f.contentsCalls()[2].query.expected_revision, undefined);
  });

  for (const action of ['household', 'playlist', 'close', 'dispose']) {
    test(kind + ': a late contents reply cannot survive ' + action + ' changes', async () => {
      const f = await fixture(kind), gate = defer();
      f.reader(async (query, page) => { const response = page(query); await gate.promise; return response; });
      const work = f.api.loadContents();
      assert.equal(await f.api.loadContents(), false, 'one bounded read at a time');
      if (action === 'household') f.select();
      else if (action === 'playlist') f.api.edit('playlist', 'SQ:8');
      else f.api[action]();
      gate.resolve(); assert.equal(await work, false); assert.equal(f.api.state.contents, null);
      assert.equal(f.contentsCalls().length, 1);
    });
  }

  test(kind + ': an empty playlist is successful, while inconsistent counters and tokens are not', async () => {
    const f = await fixture(kind); f.entries.length = 0;
    assert.equal(await f.api.loadContents(), true); assert.equal(f.api.state.contents.total, 0);
    assert.match(Phone.contentsSummary(f.api.state.contents), /empty/);
    for (const mutate of [value => { value.total = 1; }, value => { value.offset = 1; },
      value => { value.playlist.id = 'SQ:8'; }, value => { value.next_offset = 0; }, value => { value.revision = ''; }]) {
      f.reader((query, page) => { const value = page(query); mutate(value); return value; });
      assert.equal(await f.api.loadContents(), false); assert.equal(f.api.state.contents, null);
    }
  });

  test(kind + ': a renamed saved playlist cannot populate an outdated selection', async () => {
    const f = await fixture(kind); f.catalog[0].title = 'New name';
    assert.equal(await f.api.loadContents(), false); assert.equal(f.api.state.contents, null);
    await f.api.loadPlaylists(); f.api.edit('playlist', 'SQ:7');
    assert.equal(await f.api.loadContents(), true);
    assert.equal(f.api.state.contents.playlist.title, 'New name');
  });

  test(kind + ': a newly resolved coordinator cannot populate the old destination preview', async () => {
    const f = await fixture(kind);
    f.reader((query, page) => ({ ...page(query), zone: 'Office' }));
    assert.equal(await f.api.loadContents(), false);
    assert.equal(f.api.state.contents, null);
    assert.match(f.api.state.message, /changed or are unreadable/);
  });
}

test('track rendering preserves positions and treats metadata as text', () => {
  const doc = { createElement(tag) { return { tag, children: [], textContent: '', append(node) { this.children.push(node); } }; } };
  const list = { children: [], replaceChildren() { this.children = []; }, append(node) { this.children.push(node); } };
  Phone.renderContents(list, { items: [{ position: 101, title: '<img onerror=alert(1)>', creator: 'A & B', album: 'Album', duration_secs: 65 },
    { position: 102, title: '<img onerror=alert(1)>', creator: 'A & B' }] }, doc);
  assert.deepEqual(list.children.map(row => row.value), [101, 102]);
  assert.equal(list.children[0].children[0].textContent, '<img onerror=alert(1)>');
  assert.equal(list.children[0].children[1].textContent, 'A & B · Album · 1:05');
  Phone.renderContents(list, null, doc); assert.deepEqual(list.children, []);
});
