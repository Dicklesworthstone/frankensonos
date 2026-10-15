'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const { once } = require('node:events');
const fs = require('node:fs');
const path = require('node:path');
const remote = require('../crates/fsonos-api/src/remote/spotify-connect.js');

const redirect = 'http://127.0.0.1:8099/auth/spotify/callback';
const state = 'synthetic_state_123';
const status = (fields = {}) => ({ signed_in: false, pending: false, redirect_uri: redirect, ...fields });
const consent = (fields = {}, target = redirect) => ({ redirect_uri: target,
  url: `https://accounts.spotify.com/authorize?${new URLSearchParams({
    client_id: 'abc123', response_type: 'code', redirect_uri: target, code_challenge_method: 'S256',
    code_challenge: 'a'.repeat(43), scope: remote.scopes.join(' '), state, ...fields })}` });
const callback = (fields = {}, target = redirect) => `${target}${target.includes('?') ? '&' : '?'}${new URLSearchParams({ code: 'synthetic+code/=', state, ...fields })}`;
const deferred = () => { let resolve, reject; const promise = new Promise((y,n) => { resolve=y; reject=n; }); return { promise, resolve, reject }; };
const settle = () => new Promise((r) => setImmediate(r));
function harness(handler) {
  const calls=[], timers=new Map(); let serial=0, current=status();
  const normal = (name) => {
    if (name === 'spotifyConnectBegin') { current=status({pending:true}); return consent(); }
    if (name === 'spotifyConnectComplete') { current=status({signed_in:true}); return current; }
    return current;
  };
  const model=remote.model({ call: async (name, options) => {
    calls.push([name,options]); return handler ? handler(name,options,normal) : normal(name);
  }, schedule: (fn,delay) => { const id=++serial; timers.set(id,{fn,delay});return id; },cancel: (id) => timers.delete(id) });
  return {model,calls,timers,fire() { assert.equal(timers.size,1);const [id,t]=[...timers][0]; timers.delete(id);t.fn();return t.delay; }};
}

test('status validates cached and pending facts without claiming a live grant', () => {
  assert.deepEqual(remote.statusOf(status()),status());
  assert.ok(Object.isFrozen(remote.statusOf(status())));
  for (const value of [null,[],{},status({signed_in:1}),status({pending:'yes'}),
    status({redirect_uri:'javascript:alert(1)'}),status({redirect_uri:'http://localhost/callback'}),
    status({redirect_uri:'http://evil.example/callback'}),status({redirect_uri:'https://home.example/callback#'}),
    status({redirect_uri:'https://user:pass@home.example/callback'}),status({redirect_uri:redirect+'?state=reserved'})]) {
    assert.throws(()=>remote.statusOf(value));
  }
  for (const target of ['https://home.example/callback', 'http://[::1]:8099/auth/spotify/callback']) {
    assert.equal(remote.statusOf(status({redirect_uri:target})).redirect_uri,target);
  }
});

test('only the expected PKCE consent host, path, redirect and read scopes are admitted', () => {
  const result=remote.consentOf(consent(),redirect); assert.equal(result.state,state);
  assert.ok(Object.isFrozen(result)&&Object.isFrozen(result.scopes));
  for(const fields of [{response_type:'token'}, {redirect_uri:'https://evil.example/callback'},
    {code_challenge_method:'plain'}, {code_challenge:'short'}, {client_id:'a b'}, {state:''},
    {scope:'user-modify-playback-state user-library-read'}, {scope:'user-top-read'}, {extra:'unexpected'}]) {
    assert.throws(()=>remote.consentOf(consent(fields),redirect));
  }
  assert.doesNotThrow(()=>remote.consentOf(consent({scope:'user-library-read'}),redirect));
});

test('untrusted consent links are refused rather than navigated or fetched', () => {
  const original=consent();
  for(const url of [original.url.replace('https:','http:'), original.url.replace('accounts.spotify.com','accounts.spotify.com.evil.example'),
    original.url.replace('accounts.spotify.com','user:pass@accounts.spotify.com'),original.url.replace('/authorize?','/api/authorize?'),
    original.url.replace('accounts.spotify.com','accounts.spotify.com:444'), original.url+'#fragment', original.url+'&state=duplicate',
    'javascript:alert(1)', '//accounts.spotify.com/authorize', ' '+original.url, original.url+'\n']) {
    assert.throws(()=>remote.consentOf({...original,url},redirect));
  }
  assert.throws(()=>remote.consentOf({...original,redirect_uri:'https://another.example/cb'},redirect));
});

test('a matching full callback is reduced to just the two required fields', () => {
  const value=remote.callbackOf(callback(),redirect,state);
  assert.deepEqual(value,{redirect:'?code=synthetic%2Bcode%2F%3D&state=synthetic_state_123'});
  assert.ok(Object.isFrozen(value));
  const target='https://home.example/callback?from=music';
  assert.deepEqual(remote.callbackOf(callback({},target),target,state),value);
});

test('callbacks from another daemon, flow or ambiguous query never become writes', () => {
  for(const value of [callback({state:'old'}), callback({},'https://another.example/callback'),
    callback({},redirect.replace('/callback','/other')), callback()+'&code=duplicate', callback()+'#ignored',
    callback()+'&next=https%3A%2F%2Fevil.example', callback({code:''}),callback({state:''}),callback({code:'\0'}),
    '?code=x&state=y', 'javascript:alert(1)', 'x'.repeat(remote.MAX_ADDRESS+1)]) {
    assert.throws(()=>remote.callbackOf(value,redirect,state));
  }
  assert.throws(()=>remote.callbackOf(callback({from:'other'}, 'https://home.example/cb'), 'https://home.example/cb?from=music',state));
});

test('declined consent is not forwarded and its arbitrary description is not echoed', () => {
  for(const fields of ['error=access_denied&state='+state, 'error=secret-response&error_description=private&state='+state,
    'error=access_denied&code=code&state='+state]) {
    assert.throws(()=>remote.callbackOf(redirect+'?'+fields,redirect,state), (error)=> !/private|secret-response/.test(error.message));
  }
});

test('opening and checking are strictly read-only', async () => {
  const h=harness(); assert.equal(h.calls.length,0); assert.equal(await h.model.begin({}),false);
  await h.model.open(); await h.model.refresh(); assert.deepEqual(h.calls.map(c=>c[0]),['spotifyConnectStatus','spotifyConnectStatus']);
  assert.equal(h.timers.size,0); h.model.dispose();
});

test('a reviewed start uses preflight then one explicit empty-body POST', async () => {
  const h=harness();await h.model.open();const review=h.model.reviewBegin();
  assert.equal(h.calls.length,1);assert.equal(await h.model.begin(review),true);
  assert.deepEqual(h.calls.map(c=>c[0]),['spotifyConnectStatus','spotifyConnectStatus','spotifyConnectBegin']);
  assert.deepEqual(h.calls[2][1],{body:{}});assert.equal(h.model.state.status.pending,true);
  assert.equal(h.model.state.consent.state,state); h.model.dispose();
});

test('canceled or obsolete reviews cannot start a new flow', async () => {
  const h=harness();await h.model.open();const a=h.model.reviewBegin();h.model.cancelReview();
  assert.equal(await h.model.begin(a),false);const b=h.model.reviewBegin();await h.model.refresh();
  assert.equal(await h.model.begin(b),false);const c=h.model.reviewBegin();h.model.close();await h.model.open();
  assert.equal(await h.model.begin(c),false);assert.ok(h.calls.every(c=>c[0]==='spotifyConnectStatus'));h.model.dispose();
});

test('changed cached/pending facts or redirect invalidate the reviewed start', async () => {
  for(const changed of [{pending:true},{signed_in:true},{redirect_uri:'https://home.example/callback'}]) {
    let second=false;const h=harness(()=>status(second?changed:{}));await h.model.open();const review=h.model.reviewBegin();second=true;
    assert.equal(await h.model.begin(review),false);assert.match(h.model.state.message,/Nothing was submitted/);
    assert.ok(h.calls.every(c=>c[0]==='spotifyConnectStatus'));h.model.dispose();
  }
});

test('start double-clicks and refresh cannot create competing requests', async () => {
  const wait=deferred();const h=harness((name,opts,normal)=>name==='spotifyConnectBegin'?wait.promise:normal(name));
  await h.model.open();const review=h.model.reviewBegin();const first=h.model.begin(review);await settle();
  assert.equal(await h.model.begin(review),false);assert.equal(await h.model.refresh(),false);
  wait.resolve(consent());assert.equal(await first,true);await settle();
  assert.equal(h.calls.filter(c=>c[0]==='spotifyConnectBegin').length,1);h.model.dispose();
});

test('successful sign-in can complete through the manually pasted full redirect', async () => {
  const h=harness();await h.model.open();await h.model.begin(h.model.reviewBegin());
  assert.equal(await h.model.complete(callback()),true);
  const sent=h.calls.find(c=>c[0]==='spotifyConnectComplete');
  assert.deepEqual(sent[1],{body:remote.callbackOf(callback(),redirect,state)});
  assert.equal(h.model.state.status.signed_in,true);assert.equal(h.model.state.consent,null);
  assert.equal(h.timers.size,0);assert.match(h.model.state.message,/Refresh its library/);h.model.dispose();
});

test('a pending flow survives a page reload: the daemon remains final state authority', async () => {
  const h=harness((name,opts)=>name==='spotifyConnectComplete'?status({signed_in:true}):status({pending:true}));
  await h.model.open();assert.equal(h.model.state.consent,null);
  assert.equal(await h.model.complete(callback()),true);assert.equal(h.calls.filter(c=>c[0]==='spotifyConnectComplete').length,1);h.model.dispose();
});

test('a mismatched callback does not reach even preflight, and no pasted text is retained', async () => {
  const h=harness();await h.model.open();await h.model.begin(h.model.reviewBegin());const n=h.calls.length;
  assert.equal(await h.model.complete(callback({state:'private_wrong_state'})),false);
  assert.equal(h.calls.length,n);assert.ok(!JSON.stringify(h.model.state).includes('private_wrong_state'));h.model.dispose();
});

test('a duplicate completion cannot resend a one-use code', async () => {
  const wait=deferred();const h=harness((name,opts,normal)=>name==='spotifyConnectComplete'?wait.promise:normal(name));
  await h.model.open();await h.model.begin(h.model.reviewBegin());const first=h.model.complete(callback());await settle();
  assert.equal(await h.model.complete(callback()),false);wait.resolve(status({signed_in:true}));await first;
  assert.equal(h.calls.filter(c=>c[0]==='spotifyConnectComplete').length,1);h.model.dispose();
});

test('an automatically completed callback is observed without re-exchanging it', async () => {
  let completed=false;const h=harness((name,opts,normal)=>completed?status({signed_in:true}):normal(name));
  await h.model.open();await h.model.begin(h.model.reviewBegin());completed=true;
  assert.equal(await h.model.complete(callback()),false);assert.equal(h.model.state.status.signed_in,true);
  assert.equal(h.model.state.consent,null);assert.ok(!h.calls.some(c=>c[0]==='spotifyConnectComplete'));h.model.dispose();
});

test('uncertain begin and complete replies recover by reads, never automatic mutation replay', async () => {
  for(const operation of ['spotifyConnectBegin','spotifyConnectComplete']) {
    const h=harness((name,opts,normal)=>{ const answer=normal(name);if(name===operation)throw new Error('private code should not appear');return answer; });
    await h.model.open();const began=await h.model.begin(h.model.reviewBegin());
    if(operation==='spotifyConnectComplete'){assert.equal(began,true);assert.equal(await h.model.complete(callback()),false);}else assert.equal(began,false);
    await settle();assert.equal(h.model.state.uncertain,true);assert.equal(h.model.state.consent,null);
    assert.equal(h.calls.filter(c=>c[0]===operation).length,1);
    assert.equal(h.calls.at(-1)[0],'spotifyConnectStatus');assert.ok(!h.model.state.message.includes('private code'));h.model.dispose();
  }
});

test('unreadable successful replies are not treated as a working sign-in', async () => {
  const h=harness((name,opts,normal)=>name==='spotifyConnectBegin'?{url:'javascript:alert(1)',redirect_uri:redirect}:normal(name));
  await h.model.open();assert.equal(await h.model.begin(h.model.reviewBegin()),false);await settle();
  assert.equal(h.model.state.consent,null);assert.equal(h.model.state.uncertain,true);h.model.dispose();
});

test('policy, configuration and busy errors produce safe actionable fixed messages', async () => {
  for(const code of ['POLICY_DENIED','NOT_IMPLEMENTED','NOT_READY','SPOTIFY_AUTH_REQUIRED','INVALID_ARGUMENT','UNKNOWN_CODE']){
    const h=harness(()=>{throw Object.assign(new Error('PRIVATE_CALLBACK'),{code,hint:'PRIVATE_HINT'});});await h.model.open();
    assert.ok(h.model.state.message.length>30);assert.ok(!/PRIVATE/.test(h.model.state.message));
    assert.equal(h.model.state.fresh,false);assert.equal(h.model.reviewBegin(),null);h.model.dispose();
  }
});

test('running consent polling stops on completion and never starts sign-in', async () => {
  let pending=true;const h=harness(()=>status({pending,signed_in:!pending}));await h.model.open();
  assert.equal(h.fire(),3000);await settle();pending=false;h.fire();await settle();assert.equal(h.timers.size,0);
  assert.ok(h.calls.every(c=>c[0]==='spotifyConnectStatus'));h.model.dispose();
});

test('read failures retain stale state with bounded backoff', async () => {
  let fail=false;const h=harness(()=>{if(fail)throw new Error('offline');return status({pending:true});});await h.model.open();fail=true;
  await h.model.refresh();assert.equal(h.model.state.fresh,false);assert.equal(h.model.state.status.pending,true);
  const delays=[];for(let i=0;i<6;i++){delays.push(h.fire());await settle();}
  assert.deepEqual(delays,[6000,12000,24000,48000,60000,60000]);h.model.dispose();
});

test('closing clears consent addresses and halts polling without canceling server state', async () => {
  const h=harness();await h.model.open();await h.model.begin(h.model.reviewBegin());h.model.close();
  assert.equal(h.model.state.consent,null);assert.equal(h.model.state.status.pending,true);assert.equal(h.timers.size,0);
  assert.equal(await h.model.complete(callback()),false);h.model.dispose();
});

test('hiding stops reads; returning observes and never repeats sign-in writes', async () => {
  const h=harness();await h.model.open();await h.model.begin(h.model.reviewBegin());const original=h.model.state.consent;
  h.model.setVisible(false);assert.equal(h.timers.size,0);assert.equal(h.model.reviewBegin(),null);
  h.model.setVisible(true);await settle();assert.equal(h.model.state.consent,original);
  assert.equal(h.calls.at(-1)[0],'spotifyConnectStatus');assert.equal(h.calls.filter(c=>c[0]==='spotifyConnectBegin').length,1);h.model.dispose();
});

test('late reads and reopen bursts coalesce rather than resurrect stale data', async () => {
  const wait=deferred();let first=true;const h=harness(()=>{if(first){first=false;return wait.promise;}return status({pending:true});});
  const opening=h.model.open();for(let i=0;i<5;i++){h.model.close();await h.model.open();}assert.equal(h.calls.length,1);
  wait.resolve(status());await opening;await settle();assert.equal(h.calls.length,2);assert.equal(h.model.state.status.pending,true);h.model.dispose();
});

test('closing during preflight prevents POST dispatch', async () => {
  const wait=deferred();let n=0;const h=harness(()=>++n===1?status():wait.promise);await h.model.open();
  const pending=h.model.begin(h.model.reviewBegin());h.model.close();wait.resolve(status());
  assert.equal(await pending,false);assert.equal(h.calls.length,2);assert.equal(h.timers.size,0);h.model.dispose();
});

test('late sent writes after close are not published as new consent links', async () => {
  const wait=deferred();const h=harness((name,opts,normal)=>name==='spotifyConnectBegin'?wait.promise:normal(name));
  await h.model.open();const pending=h.model.begin(h.model.reviewBegin());await settle();h.model.close();wait.resolve(consent());
  assert.equal(await pending,true);assert.equal(h.model.state.consent,null);assert.equal(h.timers.size,0);h.model.dispose();
});

test('reopen during a sent write waits for its reply then rereads', async () => {
  const wait=deferred();const h=harness((name,opts,normal)=>name==='spotifyConnectBegin'?wait.promise:normal(name));
  await h.model.open();const pending=h.model.begin(h.model.reviewBegin());await settle();h.model.close();await h.model.open();
  wait.resolve(consent());await pending;await settle();assert.equal(h.calls.at(-1)[0],'spotifyConnectStatus');
  assert.equal(h.calls.filter(c=>c[0]==='spotifyConnectBegin').length,1);assert.equal(h.model.state.consent,null);h.model.dispose();
});

test('disposal suppresses all late observations', async () => {
  const wait=deferred();const h=harness(()=>wait.promise);const opening=h.model.open();h.model.dispose();wait.resolve(status({pending:true}));
  assert.equal(await opening,false);assert.equal(h.model.state.status,null);assert.equal(h.timers.size,0);assert.equal(await h.model.open(),false);
});

test('a rendering callback cannot turn an acknowledged start into a second POST', async () => {
  let pending=false;const calls=[];const m=remote.model({call:async(name)=>{calls.push(name);if(name==='spotifyConnectBegin'){pending=true;return consent();}return status({pending});},
    changed:()=>{throw new Error('view failed');},schedule:()=>0,cancel:()=>{}});
  await m.open();assert.equal(await m.begin(m.reviewBegin()),true);assert.equal(calls.filter(n=>n==='spotifyConnectBegin').length,1);m.dispose();
});

test('real loopback JSON carries the callback only to the configured daemon route', async (t) => {
  const requests=[];let pending=false,signed=false;
  const server=http.createServer(async(req,res)=>{
    let body='';for await(const chunk of req)body+=chunk;requests.push([req.method,req.url,req.headers['content-type'],body]);
    let result=status({pending,signed_in:signed});
    if(req.url==='/auth/spotify/begin'){pending=true;result=consent();}
    if(req.url==='/auth/spotify/complete'){pending=false;signed=true;result=status({signed_in:true});}
    res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(result));
  });server.listen(0,'127.0.0.1');await once(server,'listening');t.after(()=>{server.closeAllConnections();server.close();});
  const m=remote.model({call:async(name,opts)=>{
    const [method,route]=remote.routes[name];const response=await fetch(`http://127.0.0.1:${server.address().port}${route}`,{method,
      ...(method==='POST'?{headers:{'Content-Type':'application/json'},body:JSON.stringify(opts.body)}:{})});return response.json();
  }});
  await m.open();assert.equal(await m.begin(m.reviewBegin()),true);assert.equal(await m.complete(callback()),true);m.dispose();
  assert.deepEqual(requests.map(r=>r.slice(0,2)),[['GET','/auth/spotify'],['GET','/auth/spotify'],['POST','/auth/spotify/begin'],['GET','/auth/spotify'],['POST','/auth/spotify/complete']]);
  assert.equal(requests[2][3],'{}');assert.equal(requests[4][2],'application/json');
  assert.deepEqual(JSON.parse(requests[4][3]),remote.callbackOf(callback(),redirect,state));
});

test('the shipped assets and explicit library handoff are embedded under unchanged CSP', () => {
  const root=path.resolve(__dirname,'..');const embed=fs.readFileSync(path.join(root,'crates/fsonos-api/src/http/remote.rs'),'utf8');
  for(const [file,name] of [['spotify-connect.js','CONNECT_SCRIPT'],['spotify-connect.css','CONNECT_STYLE']]){
    assert.ok(embed.includes(`include_str!("../remote/${file}")`));assert.ok(embed.includes(`{${name}}`));assert.ok(embed.includes(`("${file}", ${name})`));
    const source=fs.readFileSync(path.join(root,'crates/fsonos-api/src/remote',file),'utf8');
    for(const banned of ['innerHTML','localStorage','sessionStorage','console.','eval(', 'http://','https://','password','token'])assert.ok(!source.includes(banned),`${file}: ${banned}`);
  }
  assert.ok(embed.includes("connect-src 'self'; base-uri 'none'; form-action 'none';"));
  const sync=fs.readFileSync(path.join(root,'crates/fsonos-api/src/remote/library-sync.js'),'utf8');
  assert.ok(sync.includes("document.addEventListener('fsonos-library-refresh-open', requested)"));
  assert.ok(sync.includes("document.removeEventListener('fsonos-library-refresh-open', requested)"));
});
