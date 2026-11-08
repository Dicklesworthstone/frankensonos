'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Music = require('../crates/fsonos-api/src/remote/dashboard-music.js');
const Auth = require('../crates/fsonos-api/src/remote/spotify-connect.js');
const Sync = require('../crates/fsonos-api/src/remote/library-sync.js');
const clone = value => structuredClone(value);
const callback = 'http://127.0.0.1:8099/auth/spotify/callback';
const authStatus = fields => ({ signed_in: false, pending: false, redirect_uri: callback, ...fields });
const syncStatus = fields => ({ done: 'No full refresh yet.', running: false, last: null, ...fields });
const completed = () => ({ at: 1790000000, tracks: 100, candidates: 80, classical: 12, retired: 3, notes: [] });
const intake = () => ({ pending: true, running: false, phase: 'verifying', pages: 512, tracks: 24500, verified_pages: 100, retry_at: 1790000900, not_before: 1790003600 });
const consent = () => ({ redirect_uri: callback, url: `https://accounts.spotify.com/authorize?${new URLSearchParams({
  client_id: 'synthetic123', response_type: 'code', redirect_uri: callback, code_challenge_method: 'S256',
  code_challenge: 'a'.repeat(43), scope: Auth.scopes.join(' '), state: 'synthetic_state' })}` });
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise,resolve,reject}; };
const settled = async predicate => { for(let i=0;i<100&&!predicate();i++)await new Promise(r=>setImmediate(r));assert.ok(predicate()); };

function fixture({ demo = false, request = false } = {}) {
  const calls = [], timers = new Map(); let serial = 0, override;
  const data = { auth: authStatus(), sync: syncStatus() };
  const raw = async (url, body) => {
    calls.push({url,body});
    if (override) { const value = await override(url,body); if(value!==undefined)return value; }
    if (url === '/auth/spotify') return clone(data.auth);
    if (url === '/auth/spotify/begin') { data.auth.pending=true;return consent(); }
    if (url === '/auth/spotify/complete') { data.auth=authStatus({signed_in:true});return clone(data.auth); }
    if (url === '/dj/sync') { if(body!==undefined)data.sync.running=true;return clone(data.sync); }
    throw new Error('Unexpected route: '+url);
  };
  const call = request ? C.request({fetcher:async(url,init)=>new Response(JSON.stringify(await raw(url,init.body===undefined?undefined:JSON.parse(init.body))))}) : raw;
  const control = Music.model({call,demo,schedule:(fn,ms)=>{timers.set(++serial,{fn,ms});return serial;},cancel:id=>timers.delete(id)});
  return {control,data,calls,timers,raw,override(fn){override=fn;},writes:()=>calls.filter(c=>c.body!==undefined),
    async tick(){assert.equal(timers.size,1);const [id,t]=timers.entries().next().value;timers.delete(id);t.fn();await settled(()=>!control.state.sync.loading&&!control.state.connection.loading);return t.ms;}};
}

test('music setup is lazy and opens without discovery or any selected speaker',async()=>{
  const f=fixture();assert.deepEqual(f.calls,[]);assert.equal(await f.control.start(),false);
  assert.equal(await f.control.open(),true);assert.deepEqual(f.calls,[{url:'/dj/sync',body:undefined}]);
  assert.equal(f.control.canStart(),true);assert.equal(f.timers.size,0);
  assert.equal(await f.control.open('unknown'),false);assert.equal(f.calls.length,1);f.control.dispose();
});

test('library and connection tabs each own only their visible observation loop',async()=>{
  const f=fixture();f.data.sync.running=true;await f.control.open('library');assert.equal(f.timers.size,1);
  await f.control.open('connection');assert.equal(f.timers.size,0);
  assert.deepEqual(f.calls.map(c=>c.url),['/dj/sync','/auth/spotify']);
  f.data.auth.pending=true;await f.control.refresh();assert.equal(f.timers.size,1);
  await f.control.open('library');assert.equal(f.timers.size,1);
  f.control.close();assert.equal(f.timers.size,0);assert.equal(f.writes().length,0);f.control.dispose();
});

test('refresh status retains interrupted intake and changes poll pace without claiming publication',async()=>{
  const f=fixture();f.data.sync=syncStatus({library_intake:intake()});await f.control.open();
  assert.equal(f.control.state.sync.status.last,null);
  assert.equal(f.control.state.sync.status.library_intake.tracks,24500);
  assert.equal(await f.tick(),Sync.PENDING_POLL_MS);
  f.data.sync=syncStatus({running:true,library_intake:{...intake(),running:true}});await f.tick();
  f.data.sync=syncStatus({last:completed()});assert.equal(await f.tick(),Sync.POLL_MS);
  assert.equal(f.control.state.sync.status.last.candidates,80);assert.equal(f.timers.size,0);
  assert.equal(f.writes().length,0);f.control.dispose();
});

test('one library-start request remains occupied across tab changes and prevents overlapping setup writes',async()=>{
  const f=fixture(),gate=deferred();await f.control.open();
  f.override((url,body)=>body!==undefined&&url==='/dj/sync'?gate.promise:undefined);
  const first=f.control.start();assert.equal(await f.control.start(),false);
  await f.control.open('connection');assert.equal(f.control.reviewConnect(),null);assert.equal(f.control.canConnect(),false);
  gate.resolve(syncStatus({running:true}));assert.equal(await first,true);
  assert.equal(f.control.canConnect(),true);assert.deepEqual(f.writes(),[{url:'/dj/sync',body:{}}]);f.control.dispose();
});

test('lost sync receipt recovers through status reads without another POST',async()=>{
  const f=fixture();await f.control.open();
  f.override((url,body)=>{if(body!==undefined){f.data.sync.running=true;throw new Error('Reply lost');}});
  assert.equal(await f.control.start(),false);await settled(()=>!f.control.state.sync.loading);
  assert.equal(f.control.state.sync.uncertain,true);assert.equal(f.control.state.sync.status.running,true);
  assert.match(f.control.state.sync.message,/nothing was retried/);
  f.control.suspend();assert.equal(f.timers.size,0);f.control.resume();await f.control.open();
  assert.equal(f.writes().length,1);assert.equal(await f.control.start(),false);f.control.dispose();
});

test('a reviewed sign-in preflights status, validates consent and submits one native request',async()=>{
  const f=fixture({request:true});await f.control.open('connection');const review=f.control.reviewConnect();
  assert.equal(f.writes().length,0);assert.equal(await f.control.begin(review),true);
  assert.deepEqual(f.writes(),[{url:'/auth/spotify/begin',body:{}}]);
  assert.equal(f.control.state.connection.consent.state,'synthetic_state');
  assert.equal(await f.control.begin(review),false,'a consumed review cannot be replayed');f.control.dispose();
});

test('changed sign-in facts fence the reviewed start before any write',async()=>{
  const f=fixture();await f.control.open('connection');const review=f.control.reviewConnect();
  f.data.auth.pending=true;assert.equal(await f.control.begin(review),false);
  assert.match(f.control.state.connection.message,/status changed/);assert.equal(f.writes().length,0);f.control.dispose();
});

test('callback completion sends only validated code and state to this daemon',async()=>{
  const f=fixture();await f.control.open('connection');await f.control.begin(f.control.reviewConnect());
  const before=f.writes().length;
  assert.equal(await f.control.complete('https://other.example/callback?code=private&state=synthetic_state'),false);
  assert.equal(f.writes().length,before);
  const address=callback+'?'+new URLSearchParams({code:'synthetic+code/=',state:'synthetic_state'});
  assert.equal(await f.control.complete(address),true);
  assert.deepEqual(f.writes().at(-1),{url:'/auth/spotify/complete',body:{redirect:'?code=synthetic%2Bcode%2F%3D&state=synthetic_state'}});
  assert.equal(f.control.state.connection.status.signed_in,true);
  await f.control.open('library');assert.equal(f.writes().length,2,'sign-in does not automatically sync or play');f.control.dispose();
});

test('auth failures retain stable error codes while never displaying private response text',async()=>{
  for(const [code,expected] of [['NOT_IMPLEMENTED',/FSONOS_SPOTIFY_CLIENT_ID/],['POLICY_DENIED',/house policy/],['INTERNAL',/Check status/]]){
    const call=C.request({fetcher:async()=>new Response(JSON.stringify({code,detail:'private-callback-secret',hint:'private-hint-secret'}),{status:503})});
    const timers=new Map();let seq=0;
    const control=Music.model({call,schedule:(fn,ms)=>{timers.set(++seq,{fn,ms});return seq;},cancel:id=>timers.delete(id)});
    assert.equal(await control.open('connection'),false);assert.match(control.state.connection.message,expected);
    assert.doesNotMatch(control.state.connection.message,/private-/);control.dispose();assert.equal(timers.size,0);
  }
});

test('hiding during consent creation fences the link and retains one in-flight slot',async()=>{
  const f=fixture(),gate=deferred();await f.control.open('connection');const review=f.control.reviewConnect();
  f.override((url)=>url==='/auth/spotify/begin'?gate.promise:undefined);
  const pending=f.control.begin(review);await settled(()=>f.writes().length===1);
  f.control.suspend();f.control.resume();await f.control.open('library');
  assert.equal(f.control.canStart(),false);gate.resolve(consent());assert.equal(await pending,true);
  assert.equal(f.control.state.connection.consent,null);assert.equal(f.writes().length,1);f.control.dispose();
});

test('closed or disposed controls never poll or start new work',async()=>{
  const f=fixture();f.data.auth.pending=true;await f.control.open('connection');assert.equal(f.timers.size,1);
  f.control.suspend();assert.equal(f.timers.size,0);assert.equal(await f.control.open(),false);
  assert.equal(await f.control.complete(callback),false);f.control.resume();assert.equal(f.timers.size,0);
  f.control.dispose();assert.equal(await f.control.open(),false);assert.equal(await f.control.refresh(),false);
  assert.equal(f.writes().length,0);
});

test('the design preview never reads auth, posts callbacks or fabricates a library result',async()=>{
  const f=fixture({demo:true});await f.control.open('connection');assert.equal(f.control.reviewConnect(),null);
  assert.equal(await f.control.begin({}),false);assert.equal(await f.control.complete('private-input'),false);
  await f.control.open('library');assert.equal(await f.control.start(),false);assert.equal(await f.control.refresh(),false);
  assert.equal(f.control.state.sync.status,null);assert.deepEqual(f.calls,[]);assert.equal(f.timers.size,0);f.control.dispose();
});

test('real loopback HTTP supports sign-in then an explicit independent library refresh',async t=>{
  const f=fixture(),server=http.createServer(async(req,res)=>{
    let body='';for await(const chunk of req)body+=chunk;
    const data=await f.raw(req.url,body?JSON.parse(body):undefined);
    res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(data));
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));t.after(()=>{server.closeAllConnections();return new Promise(resolve=>server.close(resolve));});
  const base='http://127.0.0.1:'+server.address().port;
  const control=Music.model({call:C.request({fetcher:(url,init)=>fetch(base+url,init)})});t.after(()=>control.dispose());
  await control.open('connection');assert.equal(await control.begin(control.reviewConnect()),true);
  assert.equal(await control.complete(callback+'?code=synthetic&state=synthetic_state'),true);
  await control.open('library');assert.equal(await control.start(),true);
  assert.deepEqual(f.writes(),[{url:'/auth/spotify/begin',body:{}},
    {url:'/auth/spotify/complete',body:{redirect:'?code=synthetic&state=synthetic_state'}},{url:'/dj/sync',body:{}}]);
  assert.ok(f.calls.every(c=>['/auth/spotify','/auth/spotify/begin','/auth/spotify/complete','/dj/sync'].includes(c.url)));
  f.control.dispose();
});

const settleMusic = async () => { for (let i = 0; i < 16; i++) await Promise.resolve(); };
test('view failures cannot repeat or prevent an acknowledged music refresh', async () => {
  const calls = [], control = Music.model({ call: async (path, body) => { calls.push([path, body]); return syncStatus(); },
    changed: () => { throw new Error('View unavailable'); } });
  await control.open(); assert.equal(await control.start(), true);
  assert.equal(calls.filter(([, body]) => body !== undefined).length, 1); control.dispose();
});

function panelHarness(answer, options = {}) {
  const nodes = [], listeners = new Map();
  class Element {
    constructor(tag) { this.tagName = tag; this.children = []; this.attrs = {}; this.events = new Map(); this.dataset = {}; this.value = ''; this.open = false; this.isConnected = true; nodes.push(this); }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(key, value) { this.attrs[key] = String(value); }
    removeAttribute(key) { delete this.attrs[key]; }
    addEventListener(key, value) { this.events.set(key, value); }
    removeEventListener(key) { this.events.delete(key); }
    focus() { doc.activeElement = this; }
    showModal() { this.open = true; }
    close() { this.open = false; this.events.get('close')?.(); }
    click() { if (!this.disabled) this.events.get('click')?.({ target: this, preventDefault() {} }); }
  }
  const doc = { hidden: false, activeElement: null, createElement: (tag) => new Element(tag),
    getElementById: (id) => nodes.find((node) => node.id === id),
    querySelectorAll: () => nodes.filter((node) => node.dataset.openMusic),
    addEventListener: (key, fn) => listeners.set(key, fn), removeEventListener: (key) => listeners.delete(key) };
  const dialog = new Element('dialog'); dialog.id = 'music-dialog';
  const launcher = new Element('button'); launcher.dataset.openMusic = 'library'; doc.activeElement = launcher;
  const calls = [], timers = new Map(); let serial = 0;
  const args = { call: async (path, body) => { calls.push([path, body]); return answer(path, body); },
    schedule: (fn, delay) => { timers.set(++serial, { fn, delay }); return serial; },
    cancel: (id) => timers.delete(id), ...options };
  const h = { calls, timers, args }; let browsed = 0;
  const panel = Music.panel({ ...h.args, document: doc, browse: () => { browsed++; } });
  return { ...h, panel, nodes, doc, dialog, launcher, byId: (id) => doc.getElementById(id), browsed: () => browsed };
}

test('the panel shows retained progress without inventing a completed refresh', async () => {
  const h = panelHarness(() => syncStatus({ library_intake: { pending: true, phase: 'verifying',
    pages: 512, tracks: 24500, verified_pages: 100, running: false, retry_at: null, not_before: null } }));
  h.panel.open('library'); await settleMusic();
  assert.equal(h.dialog.open, true); assert.equal(h.byId('music-sync-start').disabled, false);
  assert.ok(h.nodes.some((n) => n.textContent === (24500).toLocaleString()));
  assert.ok(h.nodes.some((n) => n.textContent?.includes('No completed full refresh')));
  assert.ok(h.nodes.some((n) => n.textContent?.includes('published library stays unchanged')));
  h.byId('music-browse').click(); assert.equal(h.browsed(), 1); assert.equal(h.dialog.open, false);
  assert.ok(h.calls.every(([, body]) => body === undefined)); h.panel.dispose();
});

test('callback text is cleared on submit, tab switch and suspension', async () => {
  const h = panelHarness((path) => path === '/dj/sync' ? syncStatus()
    : path === '/auth/spotify/complete' ? authStatus({ signed_in: true }) : authStatus({ pending: true }));
  h.panel.open('connection'); await settleMusic();
  h.byId('music-callback').value = callback + '?code=private-code&state=fixturestate';
  h.byId('music-connection-complete').click(); assert.equal(h.byId('music-callback').value, ''); await settleMusic();
  h.byId('music-callback').value = 'private'; h.byId('music-tab-library').click();
  assert.equal(h.byId('music-callback').value, ''); await settleMusic();
  h.panel.open('connection'); await settleMusic(); h.byId('music-callback').value = 'private';
  h.panel.suspend(); assert.equal(h.byId('music-callback').value, ''); assert.equal(h.dialog.open, false);
  h.panel.resume(); assert.equal(h.dialog.open, false); h.panel.dispose();
});

test('demo panel offers browsing but never enables connection or refresh', async () => {
  const h = panelHarness(() => { throw new Error('Unexpected request'); }, { demo: true });
  h.panel.open(); await settleMusic(); assert.equal(h.byId('music-sync-start').disabled, true);
  h.byId('music-tab-connection').click(); await settleMusic();
  assert.equal(h.byId('music-connection-begin').disabled, true);
  assert.equal(h.byId('music-connection-complete').disabled, true); assert.equal(h.calls.length, 0);
  h.panel.dispose();
});
