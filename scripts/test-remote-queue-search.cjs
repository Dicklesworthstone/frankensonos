// Run the shipped queue model; no packages or physical speakers required.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const Q = vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/queue.js'), 'utf8') + '\nQueueRemote;');
const plain = value => JSON.parse(JSON.stringify(value));
const pending = () => {
  let resolve, reject;
  const promise = new Promise((a, b) => { resolve = a; reject = b; });
  return { resolve, reject, promise };
};
function fixture(options = {}) {
  let total = options.total ?? 260, revision = 'fixture-revision', playing = 2;
  const entries = new Map(), calls = [];
  const item = position => entries.get(position) || { position, title: `Piece ${position}`,
    creator: 'Test Ensemble', album: 'Fixture recording', uri: `test:entry:${position}` };
  function page(offset, limit) {
    const count = Math.min(limit, options.short ?? limit, Math.max(0, total - offset));
    return { total, revision, playing, items: Array.from({ length: count }, (_, i) => item(offset + i + 1)) };
  }
  const call = async (name, args) => {
    calls.push(plain([name, args]));
    if (options.intercept) {
      const answer = options.intercept(name, args, calls.length, page);
      if (answer !== undefined) return answer;
    }
    if (name === 'queue') return page(args.query.offset, args.query.limit);
    if (name === 'queuePlay') return { done: 'Playing selected occurrence', changed: true };
    throw new Error('Unexpected mutation ' + name);
  };
  const control = Q.model({ zone: options.zone || 'Listening room@S2', call,
    changed: options.changed, current: options.current, blocked: options.blocked, execute: options.execute });
  return { control, calls, entries, page, item,
    setTotal: value => { total = value; }, setRevision: value => { revision = value; },
    setPlaying: value => { playing = value; } };
}

test('finds off-page titles, artists and albums with all words and accent folding', async () => {
  const f = fixture();
  f.entries.set(138, { ...f.item(138), title: 'Prélude in C', creator: 'Élodie', album: 'Evening' });
  await f.control.load();
  assert.equal(f.calls.length, 1);
  assert.equal(f.control.editSearch('ELODIE prelude'), true);
  assert.equal(f.calls.length, 1, 'typing never searches');
  assert.equal(await f.control.search(), true);
  const result = f.control.state.search;
  assert.deepEqual(plain(result.matches.map(i => i.position)), [138]);
  assert.equal(result.complete, true); assert.equal(result.scanned, 260);
  assert.equal(result.query, 'ELODIE prelude'); assert.equal(result.running, false);
  assert.equal(Object.isFrozen(result), true); assert.equal(Object.isFrozen(result.matches[0]), true);
  assert.deepEqual(f.calls.slice(1).map(([, a]) => [a.query.offset, a.query.limit]), [[0,100],[100,100],[200,60]]);
  assert.equal(f.control.state.offset, 0, 'search does not replace the displayed page');
  assert.equal(f.control.state.page.playing, 2);
});

test('repeated tracks keep occurrence positions; missing metadata does not fabricate matches', async () => {
  const f = fixture({ total: 300 });
  for (const n of [7, 108, 207]) f.entries.set(n, { position: n, title: 'Repeated music', uri: 'test:duplicate' });
  f.entries.set(111, { position: 111, title: '', uri: 'test:Repeated music' });
  await f.control.load(); await f.control.search('repeated music');
  assert.deepEqual(plain(f.control.state.search.matches.map(i => i.position)), [7,108,207]);
  assert.equal(f.calls.every(([name]) => name === 'queue'), true);
});

test('result navigation reads one current page then existing playback sends exact occurrence once', async () => {
  for (const household of ['S1', 'S2']) {
    const f = fixture({ zone: `Listening / room@${household}` });
    f.entries.set(146, { ...f.item(146), title: 'Desired piece', uri: 'test:duplicate' });
    await f.control.load(); await f.control.search('desired');
    const hit = f.control.state.search.matches[0], before = f.calls.length;
    assert.equal(await f.control.openMatch(hit), true);
    assert.equal(f.calls.length, before + 1);
    assert.equal(f.control.state.offset, 125); assert.equal(f.control.state.found, 146);
    assert.deepEqual(f.calls.at(-1), ['queue', { params: { room: `Listening / room@${household}` }, query: { offset: 125, limit: 25 } }]);
    assert.equal(await f.control.play(146), true);
    assert.deepEqual(f.calls.filter(([n]) => n !== 'queue'), [['queuePlay', { body: {
      zone: `Listening / room@${household}`, position: 146, expected_uri: 'test:duplicate', expected_revision: 'fixture-revision',
    } }]]);
  }
});

test('search result copies, old batches and edited queries cannot navigate', async () => {
  const f = fixture(); await f.control.load(); await f.control.search('piece 45');
  const hit = f.control.state.search.matches[0]; let count = f.calls.length;
  assert.equal(await f.control.openMatch({ ...hit }), false); assert.equal(f.calls.length, count);
  f.control.editSearch('piece 99');
  assert.equal(await f.control.openMatch(hit), false); assert.equal(f.calls.length, count);
  await f.control.search(); count = f.calls.length;
  assert.equal(await f.control.openMatch(hit), false); assert.equal(f.calls.length, count);
});

test('search discards prior movement consent but preserves exact selected entries', async () => {
  const f = fixture(); await f.control.load(); f.control.select(2, true); f.control.reviewMove(260);
  const review = f.control.state.pending;
  await f.control.search('piece 146');
  assert.equal(f.control.state.pending, null); assert.equal(await f.control.move(review), false);
  await f.control.openMatch(f.control.state.search.matches[0]);
  assert.equal(f.control.state.selected.has(2), true);
  assert.deepEqual(plain(f.control.removable().map(i => i.position)), [2]);
});

test('bounds results and explicitly continues after the last examined occurrence', async () => {
  const f = fixture({ total: 450 }); await f.control.load();
  await f.control.search('piece'); let first = f.control.state.search;
  assert.equal(first.matches.length, 200); assert.equal(first.scanned, 200); assert.equal(first.complete, false);
  assert.equal(await f.control.search(first.query, { ...first }), false);
  assert.equal(await f.control.search(first.query, first), true);
  let second = f.control.state.search;
  assert.equal(second.start, 200); assert.equal(second.scanned, 400); assert.equal(second.matches[0].position, 201);
  assert.equal(await f.control.openMatch(first.matches[0]), false);
  assert.equal(await f.control.search(first.query, first), false, 'old continuation cannot restart a batch');
  assert.equal(await f.control.search(second.query, second), true);
  let last = f.control.state.search;
  assert.equal(last.start, 400); assert.equal(last.matches.length, 50); assert.equal(last.complete, true);
  assert.equal(await f.control.search(last.query, last), false);
  assert.equal(await f.control.search('piece'), true, 'an explicit new search starts from the beginning');
  assert.equal(f.control.state.search.start, 0);
});

test('bounds scan requests separately from queue size and continues a no-match prefix', async () => {
  const f = fixture({ total: 10005 }); f.entries.set(10004, { ...f.item(10004), title: 'Beyond the prefix' });
  await f.control.load(); await f.control.search('beyond');
  const first = f.control.state.search;
  assert.equal(f.calls.length, 101); assert.equal(first.scanned, 10000);
  assert.equal(first.complete, false); assert.equal(first.matches.length, 0);
  await f.control.search(first.query, first);
  assert.deepEqual(plain(f.control.state.search.matches.map(i => i.position)), [10004]);
  assert.equal(f.control.state.search.complete, true);
});

test('short coherent pages advance by actual count with the same finite request budget', async () => {
  const f = fixture({ total: 110, short: 1 }); await f.control.load(); await f.control.search('unmatched');
  const first = f.control.state.search;
  assert.equal(first.scanned, 100); assert.equal(first.complete, false); assert.equal(f.calls.length, 101);
  await f.control.search(first.query, first);
  assert.equal(f.control.state.search.complete, true); assert.equal(f.calls.length, 111);
});

test('complete no-match and empty-queue searches are distinguished from limited searches', async () => {
  for (const total of [0, 11]) {
    const f = fixture({ total }); await f.control.load(); await f.control.search('absent');
    assert.equal(f.control.state.search.complete, true); assert.equal(f.control.state.search.scanned, total);
    assert.equal(f.control.state.search.matches.length, 0); assert.equal(f.calls.length, total ? 2 : 1);
  }
});

test('invalid or excessive queries make no requests and invalidate an earlier review', async () => {
  for (const query of ['', '  ', null, undefined, 3, 'a'.repeat(257), Array(34).fill('word').join(' '), '\u0301']) {
    const f = fixture(); await f.control.load(); f.control.select(1, true); f.control.reviewRemoval();
    const review = f.control.state.pending;
    assert.equal(await f.control.search(query), false); assert.equal(f.calls.length, 1);
    assert.equal(await f.control.remove(review), false); assert.equal(f.control.state.search, null);
  }
});

test('legacy queues remain browsable without a mixed-revision search', async () => {
  const f = fixture(); f.setRevision(null); await f.control.load();
  assert.equal(await f.control.search('piece'), false); assert.equal(f.calls.length, 1);
});

test('a mutation, new coordinator token or changed length between search pages discards the entire result', async () => {
  for (const change of [p => ({ ...p, revision: 'other-coordinator' }), p => ({ ...p, total: p.total + 1 }), p => ({ ...p, revision: null })]) {
    const f = fixture({ intercept: (name, args, count, page) => count === 3 ? change(page(args.query.offset, args.query.limit)) : undefined });
    await f.control.load(); assert.equal(await f.control.search('piece 9'), false);
    assert.equal(f.control.state.search, null); assert.equal(f.control.state.stale, true); assert.equal(f.calls.length, 3);
  }
});

test('contradictory visible or selected identities are rejected even with an unchanged token', async () => {
  for (const selected of [false, true]) {
    const f = fixture(); await f.control.load();
    if (selected) { f.control.select(2, true); await f.control.browse(100); }
    f.entries.set(2, { ...f.item(2), uri: 'test:replacement' });
    assert.equal(await f.control.search('piece'), false);
    assert.equal(f.control.state.search, null); assert.equal(f.control.state.selected.size, 0);
  }
});

test('malformed pages or metadata never publish an incomplete scan as usable results', async () => {
  for (const change of [() => null, p => ({ ...p, items: [] }), p => ({ ...p, items: [{ ...p.items[0], position: 2 }] }),
    p => ({ ...p, items: [{ ...p.items[0], title: {} }] }), p => ({ ...p, items: [{ ...p.items[0], uri: 42 }] }),
    p => ({ ...p, items: [{ ...p.items[0], album: 'a'.repeat(16385) }] })]) {
    const f = fixture({ intercept: (name, args, count, page) => count === 2 ? change(page(args.query.offset, args.query.limit)) : undefined });
    await f.control.load(); assert.equal(await f.control.search('piece'), false);
    assert.equal(f.control.state.search, null); assert.equal(f.calls.length, 2);
  }
});

test('missing and sentinel identities may be found but cannot open an actionable occurrence', async () => {
  for (const uri of [null, undefined, '', ' ', 'NOT_IMPLEMENTED']) {
    const f = fixture({ total: 1 }); f.entries.set(1, { ...f.item(1), uri });
    await f.control.load(); await f.control.search('piece'); const count = f.calls.length;
    assert.equal(f.control.state.search.matches.length, 1);
    assert.equal(await f.control.openMatch(f.control.state.search.matches[0]), false); assert.equal(f.calls.length, count);
  }
});

test('a result is rechecked for revision, length and exact URI before publishing its page', async () => {
  for (const change of [f => f.setRevision('changed'), f => f.setTotal(270), f => f.entries.set(146, { ...f.item(146), uri: 'test:other' })]) {
    const f = fixture(); await f.control.load(); await f.control.search('piece 146');
    const hit = f.control.state.search.matches[0]; change(f);
    assert.equal(await f.control.openMatch(hit), false);
    assert.equal(f.control.state.stale, true); assert.equal(f.control.state.search, null);
    assert.equal(f.control.state.found, null); assert.equal(f.calls.every(([n]) => n === 'queue'), true);
  }
});

test('new playback positions in the same revision do not change search identity', async () => {
  const f = fixture(); await f.control.load(); await f.control.search('piece 146');
  f.setPlaying(147); await f.control.openMatch(f.control.state.search.matches[0]);
  assert.equal(f.control.state.found, 146); assert.equal(f.control.state.page.playing, 147);
});

test('a search uses one in-flight read and excludes paging, range capture and writes', async () => {
  const wait = pending(); const f = fixture({ intercept: (name, a, count) => count === 2 ? wait.promise : undefined });
  await f.control.load(); const searching = f.control.search('piece 9');
  assert.equal(f.control.state.searching, true);
  assert.equal(await f.control.search('piece'), false); assert.equal(await f.control.load(), false);
  assert.equal(await f.control.browse(25), false); assert.equal(await f.control.showCurrent(), false);
  assert.equal(await f.control.selectRange(2, 3), false); assert.equal(await f.control.play(1), false);
  assert.equal(await f.control.add('test:source'), false); assert.equal(f.control.reviewRemoval(), false);
  assert.equal(await f.control.reconcile(), false); assert.equal(f.calls.length, 2);
  wait.resolve(f.page(0, 100)); assert.equal(await searching, true);
});

test('cancel, closure, backgrounding, loss of ownership and staleness prevent late publication and further reads', async () => {
  for (const event of ['cancel', 'close', 'suspend', 'dispose', 'stale', 'owner', 'block']) {
    const wait = pending(); let owned = true, blocked = false;
    const f = fixture({ current: () => owned, blocked: () => blocked,
      intercept: (n, a, count) => count === 2 ? wait.promise : undefined });
    await f.control.load(); const searching = f.control.search('piece');
    if (event === 'owner') owned = false;
    else if (event === 'block') { blocked = true; f.control.block(); }
    else if (event === 'cancel') f.control.cancelSearch();
    else f.control[event]();
    assert.equal(f.control.state.searching, true, 'outstanding read still owns admission');
    assert.equal(await f.control.load(), false); assert.equal(await f.control.search('piece'), false);
    wait.resolve(f.page(0, 100)); assert.equal(await searching, false);
    assert.equal(f.calls.length, 2); assert.equal(f.control.state.searching, false);
    assert.equal(f.control.state.search?.matches.length || 0, 0);
  }
});

test('cancel frees admission only after the old read drains, then a new query succeeds', async () => {
  const wait = pending(); const f = fixture({ intercept: (n, a, count) => count === 2 ? wait.promise : undefined });
  await f.control.load(); const searching = f.control.search('piece'); f.control.cancelSearch();
  assert.equal(await f.control.search('ensemble'), false);
  wait.resolve(f.page(0, 100)); await searching;
  assert.equal(await f.control.search('piece 148'), true);
  assert.equal(f.control.state.search.matches[0].position, 148);
});

test('failed reads retain an error hint and never automatically restart the search', async () => {
  const f = fixture({ intercept: (n, a, count) => {
    if (count === 3) throw Object.assign(new Error('Player did not answer'), { hint: 'Check this room.' });
  } });
  await f.control.load(); assert.equal(await f.control.search('piece 9'), false);
  assert.match(f.control.state.message, /Check this room/); assert.equal(f.control.state.search, null);
  assert.equal(f.calls.length, 3); assert.equal(await f.control.search('piece'), false);
  await f.control.load(); assert.equal(f.control.state.search, null); assert.equal(f.calls.length, 4);
});

test('search cannot bypass an uncertain write, and explicit refresh consumes old results', async () => {
  const f = fixture({ intercept: name => name === 'queuePlay' ? Promise.reject(new Error('Unknown outcome')) : undefined });
  await f.control.load(); await f.control.search('piece 9'); const hit = f.control.state.search.matches[0];
  await f.control.load(); assert.equal(await f.control.openMatch(hit), false);
  await f.control.play(1); const count = f.calls.length;
  assert.equal(f.control.state.uncertain, true); assert.equal(await f.control.search('piece'), false);
  assert.equal(f.calls.length, count);
});

test('opening a result respects the native command lane and page lifecycle', async () => {
  for (const event of ['block', 'close', 'dispose']) {
    let blocked = false, hold = false; const wait = pending();
    const f = fixture({ blocked: () => blocked, intercept: () => hold ? wait.promise : undefined });
    await f.control.load(); await f.control.search('piece 146'); const hit = f.control.state.search.matches[0];
    hold = true; const opening = f.control.openMatch(hit);
    if (event === 'block') { blocked = true; f.control.block(); } else f.control[event]();
    assert.equal(await f.control.openMatch(hit), false); wait.resolve(f.page(125, 25));
    assert.equal(await opening, false); assert.equal(f.control.state.found, null);
  }
});

test('a real localhost HTTP exchange finds a later recording then plays its existing occurrence once', async () => {
  const f = fixture({ total: 230 }); f.entries.set(177, { ...f.item(177), title: 'Evening prelude' });
  const seen = [], zone = 'Listening / room@S1';
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost'); const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null;
    seen.push({ method: req.method, path: url.pathname, body });
    res.setHeader('Content-Type', 'application/json');
    if (req.method === 'GET' && decodeURIComponent(url.pathname) === `/zones/${zone}/queue`) {
      res.end(JSON.stringify(f.page(Number(url.searchParams.get('offset')), Number(url.searchParams.get('limit')))));
    } else if (req.method === 'POST' && url.pathname === '/queue/play') {
      res.end(JSON.stringify({ done: 'Selected queue occurrence is playing', changed: true }));
    } else { res.statusCode = 404; res.end('{}'); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const base = `http://127.0.0.1:${server.address().port}`;
    const control = Q.model({ zone, call: async (route, args) => {
      const url = route === 'queue' ? `${base}/zones/${encodeURIComponent(args.params.room)}/queue?${new URLSearchParams(args.query)}` : `${base}/queue/play`;
      const response = await fetch(url, route === 'queue' ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(args.body) });
      assert.equal(response.status, 200); return response.json();
    } });
    await control.load(); assert.equal(await control.search('evening prelude'), true);
    assert.equal(await control.openMatch(control.state.search.matches[0]), true);
    assert.equal(seen.every(r => r.method === 'GET'), true, 'search and opening are entirely read-only');
    assert.equal(await control.play(177), true);
    assert.deepEqual(seen.filter(r => r.method === 'POST'), [{ method: 'POST', path: '/queue/play', body: {
      zone, position: 177, expected_uri: 'test:entry:177', expected_revision: 'fixture-revision',
    } }]);
  } finally { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
});
