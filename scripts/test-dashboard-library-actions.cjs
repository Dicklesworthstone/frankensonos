'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-library.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const id = n => String(n).padStart(22, '0');
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve,reject; const promise=new Promise((a,b)=>{resolve=a;reject=b;}); return {promise,resolve,reject}; };
const zone = h => ({household:h,coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'paused'});
const playback = z => ({zone:z,transport_state:'paused',volume:20,group_volume:22,track:{uri:'fixture:playing',queue_position:1}});
function listing(url, revision='cache-a') {
  const p=new URL(url,'http://localhost').searchParams, kind=p.get('kind'), offset=Number(p.get('offset'));
  const sourceKind=L.KINDS[kind], album=p.get('album_uri');
  const items=[{kind:sourceKind,source_uri:`spotify:${sourceKind}:${id(1)}`,title:'A cached recording',subtitle:'Synthetic performer',explicit:false,
    ...(kind==='albums'?{cached_tracks:2}:{}), ...(album?{album_uri:album,disc_number:1,track_number:1}:{})}];
  return {kind,offset,limit:50,album_uri:album,coverage:'cached',total:1,revision,next_offset:null,items:offset?[]:items};
}
async function fixture({demo=false,network=null}={}) {
  const zones=[zone('S2'),zone('S1')], states=new Map(zones.map(z=>[C.key(z),playback(z)]));
  const calls=[]; let hook=null,revision='cache-a';
  const backend=async(url,body)=>{
    calls.push({url,body:structuredClone(body)});
    if(hook){const answer=hook(url,body);if(answer!==undefined)return answer;}
    if(url==='/zones')return structuredClone(zones);
    if(url.endsWith('/state'))return structuredClone(states.get(decodeURIComponent(url.split('/')[2])));
    if(url.endsWith('/dj'))return {zone:'Studio',running:false,steering:{source:'none'},next:[]};
    if(url.startsWith('/library/browse?'))return listing(url,revision);
    if(body!==undefined)return url==='/queue'?{done:'Queued',first:3,count:2,playing_queue:false,notes:[{detail:'Playback remains paused.'}]}:{done:'Playing',changed:true};
    throw new Error(`Unexpected call: ${url}`);
  };
  const call=network?network(backend):backend;
  const controller=C.model({call,demo,now:()=>1000}), session=S.model({call,controller,demo,now:()=>1000});
  const library=L.model({call}); await controller.refresh(); await session.load(); await library.open();
  const destination=()=>({target:controller.state.selected,shape:C.shape(controller.selected().zone)});
  const choice=()=>library.choose(library.state.page.items[0]);
  const review=(action='play')=>session.reviewLibrary(choice(),action,destination());
  return {controller,session,library,calls,zones,states,call,backend,destination,choice,review,
    hook(fn){hook=fn;}, revision(v){revision=v;}, writes:()=>calls.filter(c=>c.body!==undefined)};
}

for(const kind of ['tracks','albums','playlists'])for(const action of ['play','append','next'])test(`${kind}: reviewed ${action} uses its exact native source and one shared dispatch`,async()=>{
  const f=await fixture(); await f.library.browse(kind); const r=f.review(action); assert.ok(r); assert.equal(f.writes().length,0);
  assert.equal(r.target,'Studio@S2'); assert.throws(()=>{r.body.zone='Elsewhere';},TypeError);
  assert.equal(await f.session.confirm(r),true); assert.equal(f.writes().length,1);
  assert.deepEqual(f.writes()[0],{url:action==='play'?'/play':'/queue',body:{zone:'Studio@S2',source_uri:`spotify:${L.KINDS[kind]}:${id(1)}`,title:'A cached recording',...(action==='play'?{}:{next:action==='next'})}});
  if(action!=='play')assert.match(f.controller.state.message,/Playback remains paused/);
  assert.equal(f.calls.filter(c=>c.url.startsWith('/library/browse')).length,3);
});

test('a full-album review uses the album identity, not a guessed list of cached tracks',async()=>{
  const f=await fixture(),r=f.review(); assert.match(r.detail,/full album source/); assert.match(r.detail,/2 cached tracks/);
  assert.equal(r.body.source_uri,`spotify:album:${id(1)}`); assert.equal(r.body.tracks,undefined);
  f.session.cancel(); await f.library.album(f.library.state.page.items[0]); const movement=f.review();
  assert.equal(movement.body.source_uri,`spotify:track:${id(1)}`); assert.equal(movement.catalog.query.album_uri,`spotify:album:${id(1)}`);
});

for(const mutation of ['revision','source','title','explicit','count'])test(`changed cached ${mutation} prevents dispatch instead of substituting another result`,async()=>{
  const f=await fixture(),r=f.review();
  f.hook(url=>{if(!url.startsWith('/library/browse'))return;const p=listing(url);if(mutation==='revision')p.revision='cache-b';
    if(mutation==='source')p.items[0].source_uri=`spotify:album:${id(2)}`;
    if(mutation==='title')p.items[0].title='New title';if(mutation==='explicit')p.items[0].explicit=true;if(mutation==='count')p.items[0].cached_tracks=4;return p;});
  assert.equal(await f.session.confirm(r),false); assert.equal(f.writes().length,0); assert.match(f.controller.state.message,/library selection changed/);
});

test('identical room names in different households never retarget retained catalog controls',async()=>{
  const f=await fixture(),old=f.destination(),choice=f.choice();f.controller.select('Studio@S1');
  assert.equal(f.session.reviewLibrary(choice,'play',old),null); const r=f.session.reviewLibrary(choice,'play',f.destination());
  assert.equal(await f.session.confirm(r),true);assert.equal(f.writes()[0].body.zone,'Studio@S1');
});

test('regrouping and playback changes invalidate reviewed selections',async()=>{
  for(const change of ['group','track']){
    const f=await fixture(),r=f.review(); const p=f.states.get('Studio@S2');
    if(change==='group')p.zone={...p.zone,members:['Studio']};else p.track.uri='fixture:new';
    assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
  }
});

test('an old group shape cannot create a review even when its address is unchanged',async()=>{
  const f=await fixture(),choice=f.choice(),old=f.destination();f.zones[0].members=['Studio'];f.states.set('Studio@S2',playback(f.zones[0]));
  await f.controller.refresh();assert.equal(f.session.reviewLibrary(choice,'play',old),null);assert.equal(f.writes().length,0);
});

test('a catalog action cannot replace an existing unrelated review',async()=>{
  const f=await fixture(),first=f.review();assert.equal(f.review('next'),null);assert.equal(f.session.state.pending,first);
});

test('library actions share exclusion with native controls, target changes and repeated confirmation',async()=>{
  const f=await fixture(),d=deferred(),r=f.review('next');f.hook((url,body)=>body!==undefined?d.promise:undefined);
  const pending=f.session.confirm(r);await tick();assert.equal(f.controller.state.busy,true);
  assert.equal(await f.controller.command('resume'),false);assert.equal(f.controller.select('Studio@S1'),false);assert.equal(await f.session.confirm(r),false);
  d.resolve({done:'Queued',first:1,count:1,playing_queue:false});assert.equal(await pending,true);assert.equal(f.writes().length,1);
});

for(const stop of ['cancel','hide','disconnect'])test(`${stop} during catalog preflight cannot authorize a later write`,async()=>{
  const f=await fixture(),r=f.review(),d=deferred();let query;
  f.hook(url=>{if(url.startsWith('/library/browse')){query=url;return d.promise;}});
  const run=f.session.confirm(r);await tick();assert.ok(query);
  if(stop==='cancel')f.session.cancel();else if(stop==='hide'){f.session.suspend();f.controller.suspend();}else f.controller.invalidate();
  d.resolve(listing(query));assert.equal(await run,false);assert.equal(f.writes().length,0);
});

for(const bad of [null,{}, {done:'Queued',first:0,count:1,playing_queue:false},{done:'Queued',first:1,count:0,playing_queue:false},
  {done:'Queued',first:1,count:1,playing_queue:'no'},{done:'Queued',first:1,count:1,playing_queue:false,notes:[{}]}])test(`malformed queue receipt ${JSON.stringify(bad)} stays uncertain and is not repeated`,async()=>{
  const f=await fixture(),r=f.review('append');f.hook((url,body)=>body!==undefined?Promise.resolve(bad):undefined);
  assert.equal(await f.session.confirm(r),false);assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
  f.hook(null);await f.controller.refresh();assert.equal(f.review(),null);assert.equal(f.writes().length,1);
  await f.controller.refresh(true);assert.ok(f.review());
});

test('a sent library action keeps its eventual acknowledgement after hiding without replay',async()=>{
  const f=await fixture(),d=deferred(),r=f.review('append');f.hook((url,body)=>body!==undefined?d.promise:undefined);
  const run=f.session.confirm(r);await tick();f.session.suspend();f.controller.suspend();
  d.resolve({done:'Queued after hiding',first:1,count:1,playing_queue:false});assert.equal(await run,true);
  assert.match(f.controller.state.message,/Queued after hiding/);assert.equal(f.writes().length,1);assert.equal(f.controller.canControl(),false);
});

test('preview and malformed selections never submit commands or replace pending reviews',async()=>{
  const f=await fixture({demo:true}),r=f.review();assert.ok(r);assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
  const g=await fixture();assert.equal(g.session.reviewLibrary(null,'play',g.destination()),null);
  assert.equal(g.session.reviewLibrary(g.choice(),'erase',g.destination()),null);assert.equal(g.writes().length,0);
});

test('real HTTP: album queueing is one POST and a lost reply after effect is inspected without replay',async t=>{
  let backend,postCount=0;const server=http.createServer(async(req,res)=>{
    let text='';for await(const chunk of req)text+=chunk;
    try{const data=await backend(req.url,text?JSON.parse(text):undefined);
      if(req.method==='POST'){postCount++;req.socket.destroy();return;}
      res.setHeader('content-type','application/json');res.end(JSON.stringify(data));
    }catch(error){res.statusCode=500;res.end(JSON.stringify({detail:error.message}));}
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));t.after(()=>new Promise(resolve=>{server.closeAllConnections();server.close(resolve);}));
  const f=await fixture({network:fn=>{backend=fn;return C.request({fetcher:(url,init)=>fetch(`http://127.0.0.1:${server.address().port}${url}`,init)});}});
  assert.equal(await f.session.confirm(f.review('append')),false);assert.equal(postCount,1);assert.equal(f.controller.state.uncertain,true);
  await f.controller.refresh();assert.equal(f.review(),null);assert.equal(postCount,1);
});

test('served dashboard embeds the catalog and panel before their consumers and keeps existing guards',()=>{
  const root=path.join(__dirname,'../crates/fsonos-api/src');const source=fs.readFileSync(path.join(root,'http/remote/dashboard.rs'),'utf8');
  for(const name of ['dashboard-library.js','dashboard-library-panel.js','dashboard-library.css'])assert.ok(source.includes(name));
  assert.match(source,/\{LIBRARY\}\\n\{SESSION\}/);assert.match(source,/\{LIBRARY_PANEL\}\\n\{WORKSPACE\}/);
  assert.match(source,/\{STYLE\}\\n\{LIBRARY_STYLE\}/);assert.match(source,/asset\(content_type, text\)/);assert.match(source,/microphone=\(self\)/);
  const workspace=fs.readFileSync(path.join(root,'remote/dashboard-workspace.js'),'utf8');
  assert.match(workspace,/DashboardLibraryPanel.create\(\{ call, controller, session \}\)/);
  assert.match(workspace,/library\?\.suspend\(\)/);assert.match(workspace,/library\?\.resume\(\)/);assert.match(workspace,/library\?\.observe\(\)/);
});


test('queue expansion has the finite long-write budget, without changing ordinary control/read budgets', async()=>{
  const budgets=[];const call=C.request({fetcher:async()=>new Response('{}'),schedule(fn,ms){budgets.push(ms);return budgets.length;},cancel(){}});
  await call('/queue',{zone:'Studio@S2',source_uri:`spotify:album:${id(1)}`});await call('/pause',{zone:'Studio@S2'});await call('/zones');
  assert.deepEqual(budgets,[600000,120000,10000]);
});

test('standalone session embeds still load without the optional catalog bundle',()=>{
  const vm=require('node:vm');const source=fs.readFileSync(path.join(__dirname,'../crates/fsonos-api/src/remote/dashboard-session.js'),'utf8');
  const standalone=vm.runInNewContext(source+'\nDashboardSession;', {DashboardCore:C});
  assert.equal(typeof standalone.model,'function');
  const session=standalone.model({call:async()=>[],controller:{canControl:()=>true,selected:()=>null}});
  assert.equal(session.reviewLibrary({},'play',{}),null);
});
