'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const L = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const id = (n) => String(n).padStart(22, '0');
const queryFrom = (url) => {
  const params = new URL(url, 'http://localhost').searchParams;
  assert.equal(params.get('limit'), '50');
  return L.queryOf({ kind: params.get('kind'), offset: Number(params.get('offset')),
    q: params.get('q') || '', album_uri: params.get('album_uri') });
};
function page(input = {}, total = 53, revision = 'cache-a') {
  const q = L.queryOf(input), kind = L.KINDS[q.kind];
  const items = Array.from({ length: Math.min(50, Math.max(0, total - q.offset)) }, (_, n) => ({
    kind, source_uri: `spotify:${kind}:${id(n + q.offset + 1)}`, title: `${kind} ${n + q.offset + 1}`, subtitle: 'Synthetic artist',
    explicit: false, ...(kind === 'album' ? { cached_tracks: 3 } : {}),
    ...(q.album_uri ? { album_uri: q.album_uri, disc_number: 1, track_number: n + q.offset + 1 } : {}),
  }));
  return { kind: q.kind, offset: q.offset, limit: 50, album_uri: q.album_uri, coverage: 'cached', total, revision,
    items, next_offset: q.offset + items.length < total ? q.offset + items.length : null };
}
function fixture(options = {}) {
  const calls = []; let hook = null;
  const model = L.model({ ...options, call: async (url, body) => {
    assert.equal(body, undefined, 'browsing cannot write'); calls.push(url);
    return hook ? hook(url) : page(queryFrom(url));
  } });
  return { model, calls, hook(fn) { hook = fn; } };
}

for (const kind of Object.keys(L.KINDS)) test(`real ${kind} pages preserve identities and are deeply immutable`, () => {
  const p = L.pageOf(page({ kind }), { kind });
  assert.equal(p.items.length, 50); assert.equal(p.next, 50); assert.equal(p.total, 53);
  assert.equal(p.items[0].source_uri, `spotify:${L.KINDS[kind]}:${id(1)}`);
  assert.throws(() => p.items.push({}), TypeError);
  assert.throws(() => { p.items[0].title = 'different'; }, TypeError);
  assert.throws(() => { p.query.offset = 500; }, TypeError);
});

test('the query is bounded, encoded and independent of any speaker or account grant', () => {
  const q = L.queryOf({ kind: 'tracks', q: '  A & B / 🎵  ' });
  assert.equal(L.urlOf(q), '/library/browse?kind=tracks&offset=0&limit=50&q=A+%26+B+%2F+%F0%9F%8E%B5');
  assert.equal(L.queryOf({ q: '🎵'.repeat(512) }).q.length, 1024);
  for (const value of [{kind:'favorites'}, {kind:'toString'}, {offset:-1}, {offset:1.5}, {q:'a'.repeat(513)},
    {q:4}, {kind:'albums', album_uri:`spotify:album:${id(1)}`}, {kind:'tracks',album_uri:'spotify:album:short'}]) {
    assert.throws(() => L.queryOf(value));
  }
});

for (const [field, value] of [['coverage','live'], ['kind','tracks'], ['offset',1], ['limit',49], ['total',-1],
  ['revision',''], ['revision','x'.repeat(257)], ['items',[]], ['next_offset',51], ['album_uri','other']]) {
  test(`malformed page ${field} cannot be mistaken for an empty or fresh catalog`, () => {
    const p = page(); p[field] = value; assert.throws(() => L.pageOf(p, {}));
  });
}

for (const change of [{source_uri:'spotify:track:short'}, {kind:'track'}, {explicit:null}, {cached_tracks:0},
  {title:null}, {track_number:-1}, {disc_number:1.5}]) {
  test(`malformed cached entry ${Object.keys(change)[0]} is refused`, () => {
    const p = page(); Object.assign(p.items[0],change); assert.throws(() => L.pageOf(p, {}));
  });
}

test('duplicate URIs are refused but duplicate titles retain separate source identities', () => {
  const p = page(); p.items[1].title = p.items[0].title; assert.equal(L.pageOf(p,{}).items.length,50);
  p.items[1].source_uri = p.items[0].source_uri; assert.throws(() => L.pageOf(p,{}), /duplicate/);
});

test('valid empty cache is an empty page, not a failure or a polling trigger', async () => {
  const f = fixture(); f.hook(url => page(queryFrom(url),0)); await f.model.open();
  assert.equal(f.model.state.page.total,0); assert.equal(f.model.state.stale,false);
  assert.equal(await f.model.next(),false); assert.equal(f.calls.length,1);
});

test('forward and backward navigation preserve the exact cache revision', async () => {
  const {model:m,calls} = fixture(); await m.open(); const first=m.state.page;
  assert.equal(await m.next(first),true); assert.equal(m.state.page.query.offset,50); assert.equal(m.state.page.items.length,3);
  assert.equal(await m.next(first),false,'retained controls cannot navigate a replacement page');
  assert.equal(await m.next(),false); await m.previous(); assert.equal(m.state.page.query.offset,0);
  assert.equal(calls.length,3);
});

test('cache revision change blocks pagination across hiding until explicit first-page refresh', async () => {
  const f=fixture(),m=f.model; await m.open(); const first=m.state.page;
  f.hook(url => page(queryFrom(url),53,'cache-b')); assert.equal(await m.next(),false);
  assert.equal(m.state.page,first); assert.equal(m.state.stale,true); assert.equal(m.state.resetRequired,true);
  assert.equal(m.choose(first.items[0]),null); m.suspend(); m.resume();
  assert.equal(await m.next(),false); assert.equal(m.state.resetRequired,true);
  await m.refresh(); assert.equal(m.state.page.query.offset,0); assert.equal(m.state.page.revision,'cache-b');
  assert.equal(m.state.resetRequired,false);
});

test('album drill-down uses the exact URI, clears title filters and preserves disc/track order', async () => {
  const f=fixture(),m=f.model; await m.open(); m.edit('Album name'); await m.refresh();
  const selected=m.state.page.items[2]; assert.equal(await m.album(selected),true);
  const q=queryFrom(f.calls.at(-1)); assert.equal(q.album_uri,selected.source_uri); assert.equal(q.q,'');
  assert.equal(m.state.album.title,selected.title); assert.equal(m.state.kind,'tracks');
  assert.equal(m.state.page.items[0].disc_number,1); assert.equal(m.state.page.items[0].track_number,1);
  assert.equal(m.state.page.items[0].album_uri,selected.source_uri);
  assert.equal(await m.album(selected),false,'old album row is no longer a selectable entry');
});

test('an album response cannot substitute tracks from a different album', () => {
  const q={kind:'tracks',album_uri:`spotify:album:${id(7)}`}; const p=page(q); p.items[0].album_uri=`spotify:album:${id(8)}`;
  assert.throws(()=>L.pageOf(p,q));
});

test('typing invalidates choices immediately and correcting an invalid filter restores browsing', async () => {
  const {model:m,calls}=fixture(); await m.open(); const item=m.state.page.items[0];
  m.edit('a'.repeat(513)); assert.equal(m.choose(item),null); assert.equal(await m.refresh(),false); assert.equal(calls.length,1);
  m.edit('New'); assert.equal(await m.refresh(),true); assert.equal(m.state.page.query.q,'New'); assert.equal(m.state.error,'');
});

test('one slow read admits only the latest navigation intent and never overlaps requests', async () => {
  const f=fixture(),m=f.model,gate=deferred(); let active=0,peak=0;
  f.hook(async url=> { peak=Math.max(peak,++active); if(f.calls.length===1)await gate.promise; active--; return page(queryFrom(url)); });
  const first=m.open(); await tick(); const abandoned=m.browse('tracks'); const latest=m.browse('playlists');
  assert.equal(await abandoned,false); assert.equal(f.calls.length,1);
  gate.resolve(); assert.equal(await first,false); assert.equal(await latest,true); assert.equal(peak,1);
  assert.equal(f.calls.length,2); assert.equal(m.state.page.query.kind,'playlists');
});

test('a close before dispatch starts prevents the queued read', async () => {
  const f=fixture(); const result=f.model.open(); f.model.close(); assert.equal(await result,false); assert.equal(f.calls.length,0);
});

test('closing and reopening waits for the old read, rejects its reply, then loads a fresh page', async () => {
  const f=fixture(),m=f.model,d=deferred(); f.hook(url => f.calls.length===1?d.promise:page(queryFrom(url),2));
  const first=m.open(); await tick(); m.close(); const next=m.open(); await tick(); assert.equal(f.calls.length,1);
  d.resolve(page({},53)); await first; await next; assert.equal(f.calls.length,2); assert.equal(m.state.page.total,2);
});

test('suspension retires rows and late failures without reviving or replacing current observations', async () => {
  const f=fixture(),m=f.model; await m.open(); const p=m.state.page,d=deferred(); f.hook(()=>d.promise);
  const pending=m.refresh(); await tick(); m.suspend(); d.reject(new Error('late failure')); await pending;
  assert.equal(m.state.page,p); assert.equal(m.state.error,''); assert.equal(m.choose(p.items[0]),null);
  assert.equal(await m.open(),false); m.resume(); f.hook(null); await m.refresh(); assert.equal(m.state.stale,false);
});

test('failed refresh retains the last observation but never its selection authority', async () => {
  const f=fixture(),m=f.model; await m.open(); const p=m.state.page;
  f.hook(()=>{throw Object.assign(new Error('Unavailable'),{hint:'Sync on the daemon.'});}); await m.refresh();
  assert.equal(m.state.page,p); assert.match(m.state.error,/Sync on the daemon/); assert.equal(m.choose(p.items[0]),null);
});

test('selected rows become immutable review provenance without any writes', async () => {
  const f=fixture(),m=f.model; await m.open(); const c=m.choose(m.state.page.items[0]);
  assert.equal(c.revision,'cache-a'); assert.equal(c.query.kind,'albums'); assert.equal(c.item.cached_tracks,3);
  assert.throws(()=>{c.item.title='other';},TypeError); assert.equal(m.choose({...c.item}),null); assert.equal(f.calls.length,1);
});

test('disposing resolves queued intent, rejects late replies and prevents further reads', async () => {
  const f=fixture(),m=f.model,d=deferred(); f.hook(()=>d.promise); const first=m.open(); await tick();
  const next=m.browse('tracks'); m.dispose(); assert.equal(await next,false); d.resolve(page()); await first;
  m.resume(); assert.equal(await m.open(),false); assert.equal(f.calls.length,1); assert.equal(m.state.page,null);
});

test('presentation exceptions cannot strand the serialized reader', async () => {
  const f=fixture({changed:()=>{throw new Error('render');}}); assert.equal(await f.model.open(),true);
  assert.equal(await f.model.next(),true); assert.equal(f.model.state.loading,false);
});

test('real loopback HTTP uses the existing cached-library endpoint with exact query encoding and no writes', async t=>{
  const seen=[]; const server=http.createServer((req,res)=>{
    seen.push({method:req.method,url:req.url}); const q=queryFrom(req.url);
    res.setHeader('content-type','application/json'); res.end(JSON.stringify(page(q,53)));
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  t.after(()=>new Promise(resolve=>{server.closeAllConnections();server.close(resolve);}));
  const send=C.request({fetcher:(url,init)=>fetch(`http://127.0.0.1:${server.address().port}${url}`,init)});
  const m=L.model({call:send}); await m.open(); m.edit('A & B'); await m.refresh(); await m.next();
  assert.equal(seen.length,3); assert.ok(seen.every(r=>r.method==='GET'&&r.url.startsWith('/library/browse?')));
  assert.equal(queryFrom(seen[2].url).offset,50); assert.equal(queryFrom(seen[2].url).q,'A & B');
});
