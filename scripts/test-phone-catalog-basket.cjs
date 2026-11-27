// Live Spotify selections retain their exact result and original search page;
// playback still goes through the one-shot source review and existing group lanes.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const P = require('../crates/fsonos-api/src/remote/source-player.js');
const copy = value => structuredClone(value);
const SET = 'Evening rooms', REVISION = 'a'.repeat(64);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(predicate) {
  for (let i = 0; i < 300 && !predicate(); ++i) await Promise.resolve();
  assert.ok(predicate(), 'reached the asynchronous command boundary');
}
function documentFixture() {
  const doc = { activeElement: null, hidden: false, addEventListener() {}, removeEventListener() {},
    createElement(tag) {
      const listeners = new Map();
      return { tag, ownerDocument: doc, children: [], hidden: false, disabled: false, open: false, textContent: '', value: '', attributes: {},
        append(...values) { this.children.push(...values); }, replaceChildren(...values) { this.children = values; },
        setAttribute(key, value) { this.attributes[key] = value; }, getAttribute(key) { return this.attributes[key] ?? null; },
        addEventListener(key, listener) { listeners.set(key, listener); }, removeEventListener(key) { listeners.delete(key); },
        fire(key, event = {}) { return listeners.get(key)?.({ target: this, preventDefault() {}, ...event }); },
        focus() { doc.activeElement = this; }, scrollIntoView() {},
        contains(node) { return this === node || this.children.some(child => child?.contains?.(node)); },
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      };
    },
  };
  const el = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key === 'class') node.className = value;
      else if (key.startsWith('aria-')) node.setAttribute(key, value);
      else node[key] = value;
    }
    node.append(...children); return node;
  };
  return { doc, el };
}
const descendants = node => [node, ...(node.children || []).flatMap(child => child && typeof child === 'object' ? descendants(child) : [])];

async function fixture({ dom = false } = {}) {
  const room = (household, name, player) => ({ household, room: name, player, coordinator: player });
  const physical = [
    { household: 'S1', coordinator: 'S1_DEN', coordinator_room: 'Den', members: [room('S1', 'Den', 'S1_DEN')] },
    { household: 'S2', coordinator: 'S2_STUDY', coordinator_room: 'Study', members: [room('S2', 'Study', 'S2_STUDY')] },
  ];
  const zones = physical.map(group => ({ household: group.household, coordinator_room: group.coordinator_room,
    members: group.members.map(member => member.room), transport_state: 'paused' }));
  const states = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, {
    zone, transport_state: 'paused', track: { title: 'Existing track', uri: 'fixture:current', queue_position: 2, position_secs: 15 },
  }]));
  const queues = new Map(zones.map(zone => [zone.coordinator_room + '@' + zone.household, {
    zone: zone.coordinator_room, total: 4, playing: 2, revision: 'queue:' + zone.household,
    items: [{ position: 1, title: 'Queue head', uri: 'fixture:head' }],
  }]));
  const h = { physical, zones, states, queues, calls: [], pages: new Map(), custom: null, executions: [], response: null };
  h.groupsFor = target => target === SET ? physical : physical.filter(group => group.coordinator_room + '@' + group.household === target);
  h.pageKey = query => JSON.stringify([query.q, query.kind, query.offset || 0]);
  h.page = query => {
    const key = h.pageKey(query);
    if (!h.pages.has(key)) {
      const offset = query.offset || 0, seed = [...query.q].reduce((sum, char) => sum + char.codePointAt(0), 0) * 10000;
      h.pages.set(key, { query: query.q, kind: query.kind, offset, more: offset < 20, next_offset: offset < 20 ? offset + 10 : null,
        items: Array.from({ length: offset < 20 ? 10 : 5 }, (_, index) => ({
          source_uri: 'spotify:' + query.kind + ':' + String(seed + offset + index).padStart(22, '0'),
          title: query.q + ' result ' + (offset + index + 1), subtitle: query.kind === 'episode' ? 'Podcast series' : 'Artist · Album',
        })) });
    }
    return copy(h.pages.get(key));
  };
  h.receipt = body => {
    const groups = h.groupsFor(body.zone), count = body.source.tracks.length;
    if (body.mode === 'play') return { done: 'Selected Spotify items playing', changed: true,
      ...(groups.length > 1 ? { batch: { complete: true, results: groups.map(group => ({
        household: group.household, target: group.coordinator, zone: group.coordinator_room,
        result: { status: 'succeeded', done: 'Started selected items', changed: true },
      })) } } : {}) };
    const added = groups.map(group => ({ household: group.household, coordinator: group.coordinator,
      queue: { done: 'Selected Spotify items queued', zone: group.coordinator_room,
        first: body.mode === 'next' ? 3 : 5, count, playing_queue: true } }));
    return groups.length === 1 ? added[0].queue
      : { done: 'Added selection to groups', complete: true, added, failed: [], logged: true, undoable: true };
  };
  h.call = async (name, args = {}) => {
    h.calls.push([name, args]);
    if (h.custom) { const value = h.custom(name, args); if (value !== undefined) return value; }
    let value;
    if (name === 'zones') value = zones;
    else if (name === 'sourceSpotifySearch') value = h.page(args.query);
    else if (name === 'sourceTargetResolve') {
      const groups = h.groupsFor(args.query.target);
      value = { target: args.query.target, matched: args.query.target === SET ? 'alias' : 'room', revision: REVISION,
        rooms: groups.flatMap(group => group.members), groups };
    } else if (name === 'state') value = states.get(args.params.room);
    else if (name === 'sourceQueueState') value = queues.get(args.params.room);
    else if (name === 'sourceControl') {
      assert.equal(args.body.source.kind, 'tracks', 'one exact track array, not an album or playlist expansion');
      value = h.response || h.receipt(args.body);
    } else throw new Error('Unexpected route ' + name);
    return copy(value);
  };
  const view = dom ? documentFixture() : null;
  const options = { call: h.call, executeSet: async (groups, work) => { h.executions.push(groups); return work(); } };
  h.api = dom ? P.panel({ ...options, el: view.el, document: view.doc }) : P.model(options); h.doc = view?.doc;
  await h.api.refresh(); h.api.edit('sourceType', 'catalog');
  h.writes = () => h.calls.filter(([, args]) => args.body !== undefined);
  h.search = async (q = 'Evening & friends', kind = 'track') => {
    h.api.catalog.edit('kind', kind); h.api.catalog.edit('query', q);
    assert.equal(await h.api.catalog.search(), true, h.api.catalog.state.message);
    return h.api.catalog.state.items;
  };
  h.chooseDestination = target => {
    if (target === SET) { h.api.edit('targetMode', 'set'); h.api.edit('targetQuery', target); }
    else h.api.edit('target', target);
  };
  return h;
}
async function fillBasket(h) {
  let items = await h.search(); const first = items[1]; assert.equal(h.api.catalog.toggle(first, items), true);
  assert.equal(await h.api.catalog.next(), true); items = h.api.catalog.state.items; const second = items[3];
  assert.equal(h.api.catalog.toggle(second, items), true);
  items = await h.search('Conversations', 'episode'); const episode = items[2]; assert.equal(h.api.catalog.toggle(episode, items), true);
  assert.equal(h.api.catalog.editSelection('up', 2), true);
  assert.equal(h.api.catalog.editSelection('repeat', 1), true);
  assert.equal(h.api.catalog.editSelection('up', 3), true);
  assert.deepEqual(h.api.catalog.state.basket.map(row => row.item.source_uri),
    [first.source_uri, episode.source_uri, second.source_uri, episode.source_uri]);
  return { first, second, episode };
}

for (const mode of ['play', 'next', 'end']) for (const target of ['Den@S1', SET]) {
  test('phone live catalog basket: ' + mode + ' sends one ordered mixed selection to ' + target, async () => {
    const h = await fixture(), chosen = await fillBasket(h), basket = h.api.catalog.state.basket;
    assert.equal(h.api.state.target, '', 'music can be chosen before its destination');
    h.chooseDestination(target); assert.equal(h.api.catalog.state.basket, basket);
    const begin = h.calls.length; assert.equal(await h.api.reviewCatalog(mode), true, h.api.state.message); const p = h.api.state.pending;
    assert.ok(Object.isFrozen(p.body.source.tracks)); assert.equal(p.catalogBasket, basket);
    assert.equal(p.targets.groups.length, target === SET ? 2 : 1); assert.equal(h.writes().length, 0);
    assert.equal(await h.api.confirm(p), true, h.api.state.result?.detail); assert.equal(await h.api.confirm(p), false);
    assert.deepEqual(h.writes(), [['sourceControl', { body: { zone: target, expected_revision: REVISION,
      mode: mode === 'end' ? 'append' : mode, source: { kind: 'tracks', tracks: [
        chosen.first, chosen.episode, chosen.second, chosen.episode,
      ].map(({ source_uri, title }) => ({ source_uri, title })) } } }]]);
    assert.equal(h.executions.length, 1);
    const searches = h.calls.slice(begin).filter(([name]) => name === 'sourceSpotifySearch').map(([, args]) => args.query);
    assert.equal(searches.length, 6, 'each distinct original page is checked once per review and confirmation');
    for (let index = 0; index < 3; index++) assert.deepEqual(searches[index], searches[index + 3]);
    assert.deepEqual(searches.map(q => [q.q, q.kind, q.offset]), [
      ['Evening & friends', 'track', 0], ['Conversations', 'episode', 0], ['Evening & friends', 'track', 10],
      ['Evening & friends', 'track', 0], ['Conversations', 'episode', 0], ['Evening & friends', 'track', 10],
    ]);
    assert.ok(searches.every(q => q.limit === 10 && !Object.hasOwn(q, 'expected_revision')), 'live search uses no invented cached revision');
    assert.equal(h.api.catalog.state.basket, basket); assert.equal(h.states.get('Den@S1').transport_state, 'paused');
  });
}

test('phone live catalog basket: the normal Review button uses the selected order and mode', async () => {
  const h = await fixture(); await fillBasket(h); h.chooseDestination('Den@S1'); h.api.edit('mode', 'end');
  assert.equal(await h.api.review(), true); assert.equal(h.api.state.pending.body.mode, 'append');
  assert.equal(h.api.state.pending.body.source.tracks.length, 4); assert.equal(h.writes().length, 0);
});

test('phone live catalog basket: result ranking can change within its original page', async () => {
  const h = await fixture(); await fillBasket(h); h.chooseDestination('Den@S1');
  for (const page of h.pages.values()) page.items.reverse();
  assert.equal(await h.api.reviewCatalog('next'), true, h.api.state.message);
  assert.equal(await h.api.confirm(h.api.state.pending), true, h.api.state.result?.detail);
  assert.equal(h.writes().length, 1);
});

test('phone live catalog basket: changed or unavailable original results block every write', async () => {
  for (const phase of ['review', 'confirm']) for (const change of ['URI', 'title', 'subtitle', 'missing', 'auth']) {
    const h = await fixture(); await fillBasket(h); h.chooseDestination(SET);
    let p;
    if (phase === 'confirm') { assert.equal(await h.api.reviewCatalog('play'), true); p = h.api.state.pending; }
    const key = h.pageKey({ q: 'Evening & friends', kind: 'track', offset: 10 }), page = h.pages.get(key);
    if (change === 'URI') page.items[3].source_uri = 'spotify:track:' + 'Z'.repeat(22);
    if (change === 'title') page.items[3].title = 'A different result';
    if (change === 'subtitle') page.items[3].subtitle = 'Another artist';
    if (change === 'missing') page.items.splice(3, 1);
    if (change === 'auth') h.custom = name => {
      if (name === 'sourceSpotifySearch') throw Object.assign(new Error('Sign in to Spotify again'), { hint: 'Reconnect the owner account.' });
    };
    assert.equal(phase === 'review' ? await h.api.reviewCatalog('play') : await h.api.confirm(p), false, phase + ' ' + change);
    assert.equal(h.writes().length, 0); assert.equal(h.api.state.pending, null);
    assert.match(phase === 'review' ? h.api.state.message : h.api.state.result.detail,
      change === 'auth' ? /Reconnect/ : /original page/);
  }
});

test('phone live catalog basket: stale row and basket handlers cannot select a replacement result', async () => {
  const h = await fixture(), items = await h.search(), item = items[0];
  h.api.catalog.toggle(item, items); const basket = h.api.catalog.state.basket;
  await h.api.catalog.next();
  assert.equal(h.api.catalog.toggle(item, items), false);
  assert.equal(h.api.catalog.editSelection('repeat', 0, basket), true); const current = h.api.catalog.state.basket;
  assert.equal(h.api.catalog.editSelection('remove', 0, basket), false);
  assert.equal(h.api.catalog.editSelection('up', 1, basket), false);
  h.chooseDestination('Den@S1'); assert.equal(await h.api.reviewCatalog('next', basket), false);
  assert.equal(h.api.catalog.state.basket, current); assert.equal(h.writes().length, 0);
});

test('phone live catalog basket: destination edits keep music while all catalog edits retire prior consent', async () => {
  for (const edit of ['query', 'kind', 'repeat', 'remove', 'clear', 'destination', 'mode']) {
    const h = await fixture(); await fillBasket(h); h.chooseDestination('Den@S1');
    assert.equal(await h.api.reviewCatalog('next'), true); const p = h.api.state.pending, old = h.api.catalog.state.basket;
    if (edit === 'query') h.api.catalog.edit('query', 'New query');
    if (edit === 'kind') h.api.catalog.edit('kind', 'track');
    if (edit === 'repeat') h.api.catalog.editSelection('repeat', 0);
    if (edit === 'remove') h.api.catalog.editSelection('remove', 0);
    if (edit === 'clear') h.api.catalog.clearSelection();
    if (edit === 'destination') h.api.edit('target', 'Study@S2');
    if (edit === 'mode') h.api.edit('mode', 'end');
    assert.equal(h.api.state.pending, null, edit); assert.equal(await h.api.confirm(p), false, edit);
    if (['query', 'kind', 'destination', 'mode'].includes(edit)) assert.equal(h.api.catalog.state.basket, old, edit);
    assert.equal(h.writes().length, 0);
  }
});

test('phone live catalog basket: query navigation and cancellation retain selected provenance', async () => {
  const h = await fixture(), items = await h.search(); h.api.catalog.toggle(items[1]); const basket = h.api.catalog.state.basket;
  assert.equal(await h.api.catalog.next(), true); assert.equal(await h.api.catalog.previous(), true);
  assert.equal(h.api.catalog.state.basket, basket);
  h.api.catalog.edit('query', 'Another query'); const gate = deferred(); let waiting = false;
  h.custom = (name, args) => {
    if (name === 'sourceSpotifySearch') { waiting = true; const page = h.page(args.query); return gate.promise.then(() => page); }
  };
  const searching = h.api.catalog.search(); await until(() => waiting); h.api.catalog.cancel(); gate.resolve();
  assert.equal(await searching, false); assert.equal(h.api.catalog.state.basket, basket);
  assert.deepEqual(h.api.catalog.state.items, []); assert.equal(h.writes().length, 0);
});

test('phone live catalog basket: ten includes repeats and removing one leaves the remaining occurrences', async () => {
  const h = await fixture(), items = await h.search(); h.api.catalog.toggle(items[0]);
  for (let i = 1; i < 10; i++) assert.equal(h.api.catalog.editSelection('repeat', 0), true);
  const full = h.api.catalog.state.basket; assert.equal(full.length, 10);
  assert.equal(h.api.catalog.editSelection('repeat', 0), false); assert.equal(h.api.catalog.toggle(items[1]), false);
  assert.equal(h.api.catalog.state.basket, full);
  assert.equal(h.api.catalog.editSelection('remove', 4), true); assert.equal(h.api.catalog.toggle(items[1]), true);
  assert.equal(h.api.catalog.state.basket.length, 10);
  assert.equal(h.api.catalog.toggle(items[0]), true); assert.equal(h.api.catalog.state.basket.length, 1);
  assert.equal(h.api.catalog.state.basket[0].item, items[1]);
  assert.equal(h.api.catalog.editSelection('remove', 0), true); assert.deepEqual(h.api.catalog.state.basket, []);
});

test('phone live catalog basket: containers keep their individual Use source action', async () => {
  for (const kind of ['album', 'playlist', 'artist', 'show']) {
    const h = await fixture(); await fillBasket(h); const old = h.api.catalog.state.basket;
    const items = await h.search('Container', kind);
    assert.equal(h.api.catalog.toggle(items[0]), false); assert.equal(h.api.catalog.state.basket, old);
    assert.equal(h.api.chooseCatalog(items[0], items), true); assert.equal(h.api.state.sourceType, 'link');
    assert.equal(h.api.state.input, items[0].source_uri); assert.deepEqual(h.api.catalog.state.basket, []);
    assert.equal(h.writes().length, 0);
  }
});

test('phone live catalog basket: source changes and close clear selections, while imports cannot overwrite them', async () => {
  const h = await fixture(); await fillBasket(h); const basket = h.api.catalog.state.basket;
  assert.equal(await h.api.offer({ source_uri: 'spotify:track:' + 'Z'.repeat(22) }), false);
  assert.equal(h.api.catalog.state.basket, basket);
  h.api.edit('sourceType', 'link'); assert.deepEqual(h.api.catalog.state.basket, []);
  h.api.edit('sourceType', 'catalog'); const items = await h.search(); h.api.catalog.toggle(items[0]); h.api.close();
  assert.deepEqual(h.api.catalog.state.basket, []); assert.equal(h.writes().length, 0);
});

test('phone live catalog basket: a late original-page check cannot restore consent after closing or changing destination', async () => {
  for (const action of ['close', 'destination']) {
    const h = await fixture(); await fillBasket(h); h.chooseDestination(SET);
    const gate = deferred(); let waiting = false, reads = 0;
    h.custom = (name, args) => {
      if (name === 'sourceSpotifySearch') { waiting = true; reads++; const page = h.page(args.query); return gate.promise.then(() => page); }
    };
    const reviewing = h.api.reviewCatalog('play'); await until(() => waiting);
    if (action === 'close') h.api.close(); else h.api.edit('targetQuery', 'Den@S1');
    gate.resolve(); assert.equal(await reviewing, false); assert.equal(reads, 1);
    assert.equal(h.api.state.pending, null); assert.equal(h.writes().length, 0);
  }
});

test('phone live catalog basket: changed destination playback or queue invalidates the final review', async () => {
  for (const change of ['queue', 'playback', 'group']) {
    const h = await fixture(); await fillBasket(h); h.chooseDestination('Den@S1'); await h.api.reviewCatalog('next'); const p = h.api.state.pending;
    if (change === 'queue') h.queues.get('Den@S1').revision = 'changed';
    if (change === 'playback') h.states.get('Den@S1').track.queue_position++;
    if (change === 'group') h.physical[0].coordinator = 'REPLACEMENT';
    assert.equal(await h.api.confirm(p), false, change); assert.equal(h.writes().length, 0);
  }
});

test('phone live catalog basket: a partial receipt survives closing and never repeats successful groups', async () => {
  const h = await fixture(); await fillBasket(h); h.chooseDestination(SET); await h.api.reviewCatalog('next'); const p = h.api.state.pending;
  const reply = h.receipt(p.body), failed = reply.added.pop();
  reply.complete = false; reply.undoable = false;
  reply.failed.push({ household: failed.household, coordinator: failed.coordinator, zone: failed.queue.zone,
    error: { code: 'QUEUE_EDIT_UNCERTAIN', detail: 'The speaker reply was lost', retryable: false } });
  const gate = deferred(); h.custom = name => name === 'sourceControl' ? gate.promise.then(() => reply) : undefined;
  const sending = h.api.confirm(p); await until(() => h.writes().length === 1);
  assert.equal(await h.api.confirm(p), false); assert.equal(h.api.catalog.editSelection('remove', 0), false);
  h.api.close(); gate.resolve(); assert.equal(await sending, true);
  assert.equal(h.api.state.result.partial, true); assert.match(h.api.state.result.detail, /reply was lost/);
  assert.equal(h.writes().length, 1); assert.equal(await h.api.confirm(p), false);
});

test('phone live catalog basket: missing replies and invalid receipt counts remain uncertain after one send', async () => {
  for (const fault of ['lost', 'count']) {
    const h = await fixture(); await fillBasket(h); h.chooseDestination(SET); await h.api.reviewCatalog('end'); const p = h.api.state.pending;
    h.custom = (name, args) => {
      if (name !== 'sourceControl') return;
      if (fault === 'lost') throw new Error('Reply lost');
      const reply = h.receipt(args.body); reply.added[0].queue.count--; return reply;
    };
    assert.equal(await h.api.confirm(p), false); assert.equal(h.api.state.result.status, 'uncertain');
    assert.equal(await h.api.confirm(p), false); assert.equal(h.writes().length, 1);
  }
});

test('phone live catalog basket DOM shows exact URI, search provenance, editing and one-shot consent', async () => {
  const h = await fixture({ dom: true }), all = () => descendants(h.api.node);
  const query = { q: 'Evening & friends', kind: 'track', offset: 0, limit: 10 };
  h.page(query); h.pages.get(h.pageKey(query)).items[1].title = '<img src=x onerror=alert(1)> Selected song';
  let items = await h.search();
  const select = all().find(node => node.getAttribute?.('data-catalog-select') === items[1].source_uri);
  select.focus(); select.fire('click'); assert.equal(h.doc.activeElement.getAttribute('data-catalog-select'), items[1].source_uri);
  const panel = all().find(node => node.getAttribute?.('aria-label') === 'Selected Spotify tracks and episodes');
  const list = () => descendants(panel).find(node => node.getAttribute?.('aria-label') === 'Spotify selection in playback order');
  const action = (verb, index) => descendants(list()).find(node => node.getAttribute?.('data-catalog-basket-action') === verb
    && node.getAttribute?.('data-catalog-basket-index') === String(index));
  const play = descendants(panel).find(node => node.tag === 'button' && node.textContent === 'Play selected now');
  assert.equal(play.disabled, true, 'selection can be edited before choosing where to play');
  action('repeat', 0).fire('click'); assert.equal(h.api.catalog.state.basket.length, 2);
  const oldRemove = action('remove', 1); action('up', 1).fire('click'); const basket = h.api.catalog.state.basket;
  oldRemove.fire('click'); assert.equal(h.api.catalog.state.basket, basket);
  const text = descendants(panel).map(node => node.textContent).join('\n');
  assert.match(text, /spotify:track:/); assert.match(text, /Search “Evening & friends”/); assert.match(text, /results from 1/);
  assert.match(text, /<img src=x onerror=alert\(1\)> Selected song/); assert.equal(all().some(node => node.tag === 'img'), false);
  h.chooseDestination('Den@S1'); assert.equal(play.disabled, false); play.fire('click'); await until(() => !!h.api.state.pending);
  const consent = all().find(node => node.getAttribute?.('aria-label') === 'Reviewed Spotify selection in playback order');
  assert.equal(consent.children.length, 2);
  assert.ok(descendants(consent).some(node => node.textContent === items[1].source_uri));
  const review = all().find(node => node.getAttribute?.('aria-label') === 'Review selected playback');
  const confirm = descendants(review).find(node => node.tag === 'button' && node.textContent === 'Play now');
  const gate = deferred(); h.custom = (name, args) => name === 'sourceControl' ? gate.promise.then(() => h.receipt(args.body)) : undefined;
  confirm.fire('click'); await until(() => h.writes().length === 1);
  assert.equal(play.disabled, true); assert.ok(descendants(list()).filter(node => node.getAttribute?.('data-catalog-basket-action')).every(node => node.disabled));
  gate.resolve(); await until(() => h.api.state.result?.status === 'accepted'); assert.equal(h.writes().length, 1);
});
