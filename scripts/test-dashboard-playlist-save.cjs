'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Save = require('../crates/fsonos-api/src/remote/dashboard-playlist-save.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise,resolve,reject}; };
const settled = async predicate => { for(let i=0;i<200&&!predicate();i++) await new Promise(resolve=>setImmediate(resolve)); assert.ok(predicate()); };

function fixture({demo=false,network,changed=()=>{}}={}) {
  const zones=['S2','S1'].map((household,i)=>({household,coordinator_room:'Den',members:i?['Den']:['Den','Side'],transport_state:'playing'}));
  const rows=new Map(zones.map(zone=>[C.key(zone),{zone,transport_state:'playing',volume:20,mute:false,
    track:{uri:'fixture:current',title:'Current song',queue_position:2,position_secs:37,duration_secs:300}}]));
  const queues=new Map(zones.map(zone=>[C.key(zone),{zone:'Den',total:3,revision:'queue:'+zone.household,playing:2,
    items:[{position:1,uri:'fixture:first',title:'First song'}]}]));
  const catalogs=new Map(['S1','S2'].map(home=>['Den@'+home,[{id:'SQ:7',title:'Evening '+home}]]));
  const calls=[],saved=[];let time=1000,reader=null,writer=null,control,after=null;
  const read=url=>{
    if(url==='/zones')return copy(zones);
    if(url.startsWith('/sonos/playlists?'))return copy(catalogs.get(new URLSearchParams(url.split('?')[1]).get('zone')));
    const target=decodeURIComponent(url.split('/')[2]||'');
    if(url.endsWith('/state'))return copy(rows.get(target));
    if(url.endsWith('/queue?offset=0&limit=1'))return copy(queues.get(target));
    throw new Error('Unexpected read '+url);
  };
  const write=(url,body)=>{
    assert.equal(url,'/sonos/playlists/save');
    const catalog=catalogs.get(body.zone),playlist={id:'SQ:'+(8+catalog.length-1),title:body.title};
    catalog.push(playlist);
    return {done:'Saved the queue as Sonos playlist '+playlist.id,zone:'Den',playlist,logged:true};
  };
  const raw=async(url,body,options)=>{
    calls.push({url,body,options});
    if(body!==undefined)return writer?writer(url,body,options,write):write(url,body);
    return reader?reader(url,options,read):read(url);
  };
  const call=network?network(raw):raw;
  const controller=C.model({call,demo,now:()=>time,changed:()=>control?.observe()});
  control=Save.model({call,controller,now:()=>time,changed,saved:async value=>{saved.push(value);if(after)await after(value);}});
  return {controller,control,zones,rows,queues,catalogs,calls,saved,raw,
    read(fn){reader=fn;},write(fn){writer=fn;},after(fn){after=fn;},time(value){time=value;},
    writes:()=>calls.filter(call=>call.body!==undefined),
    async ready(){assert.equal(await controller.refresh(),true);controller.connection(true);return this;}};
}
async function review(f,title='  Evening & Friends  ') {
  assert.equal(f.control.open(),true);assert.equal(await f.control.read(),true);
  assert.equal(f.control.edit('title',title),true);const proposed=await f.control.review();assert.ok(proposed);return proposed;
}

test('titles match native whitespace and UTF-8 bounds without truncation',()=>{
  assert.equal(Save.titleOf('  Dvořák & friends  '),'Dvořák & friends');
  assert.equal(Save.titleOf('\u0085Mix\u0085'),'Mix');
  assert.equal(Save.titleOf('\ufeffMix\ufeff'),'\ufeffMix\ufeff');
  assert.equal(Save.titleOf('\ufeff'),'\ufeff');
  assert.equal(Save.titleOf('é'.repeat(511)+'x').length,512);
  assert.equal(Save.titleOf('🎵'.repeat(255)+'abc').length,513);
  for(const title of ['', ' \t ', '\u0085', 'A\nB', 'A\0B', 'A\u007fB', 'A\u0085B', 'é'.repeat(512),'🎵'.repeat(256),'x'.repeat(1024),'\ud800']) {
    assert.throws(()=>Save.titleOf(title),Error,title);
  }
});

test('opening is lazy and every observation is bounded and read-only',async()=>{
  const f=await fixture().ready(),start=f.calls.length;
  assert.equal(f.control.open(),true);assert.equal(f.calls.length,start);
  assert.equal(f.control.canReview(),false);assert.equal(await f.control.read(),true);
  assert.deepEqual(f.calls.slice(start).map(call=>call.url),['/zones','/zones/Den%40S2/queue?offset=0&limit=1']);
  assert.deepEqual(f.control.state.queue,{total:3,revision:'queue:S2',playing:2});
  assert.equal(f.control.state.loaded,true);assert.equal(f.writes().length,0);
});

test('empty or malformed queues cannot authorize a save',async()=>{
  for(const queue of [
    {zone:'Den',total:0,revision:'empty',playing:null,items:[]},
    {zone:'Den',total:3,revision:'',playing:null,items:[{position:1}]},
    {zone:'Other',total:3,revision:'changed',playing:1,items:[{position:1}]},
    {zone:'Den',total:3,revision:'missing-first',playing:1,items:[]},
  ]) {
    const f=await fixture().ready();f.queues.set('Den@S2',queue);f.control.open();await f.control.read();
    f.control.edit('title','Mix');assert.equal(await f.control.review(),null);assert.equal(await f.control.apply(),false);
    assert.equal(f.writes().length,0);
  }
});

test('title failures precede any additional observation or write',async()=>{
  const f=await fixture().ready();f.control.open();await f.control.read();const count=f.calls.length;
  for(const title of ['', 'A\nB', 'é'.repeat(512)]) {
    f.control.edit('title',title);assert.equal(f.control.canReview(),false);assert.equal(await f.control.review(),null);
    assert.equal(f.calls.length,count);
  }
  assert.equal(f.control.edit('target','Other@S2'),false);assert.equal(f.writes().length,0);
});

test('a frozen reviewed queue saves once with a qualified target and no playback refresh',async()=>{
  const f=await fixture().ready(),p=await review(f),before=copy([...f.rows]),start=f.calls.length;
  for(const value of [p,p.group,p.queue,p.body])assert.ok(Object.isFrozen(value));
  assert.match(p.description,/new Sonos playlist/);assert.match(p.description,/radio/);assert.match(p.description,/another controller/);
  assert.equal(await f.control.apply(p),true);assert.equal(await f.control.apply(p),false);
  assert.deepEqual(f.calls.slice(start).map(call=>call.url),['/zones','/zones/Den%40S2/queue?offset=0&limit=1','/sonos/playlists/save']);
  assert.deepEqual(f.writes().map(({url,body})=>({url,body})),[{url:'/sonos/playlists/save',body:{zone:'Den@S2',title:'Evening & Friends'}}]);
  assert.deepEqual([...f.rows],before);assert.equal(f.controller.selected().fresh,true);
  assert.equal(f.control.state.result.status,'accepted');assert.equal(f.control.state.result.playlist.id,'SQ:8');
  assert.equal(f.control.state.result.target,'Den@S2');assert.equal(f.control.state.loaded,false);
  assert.deepEqual(f.saved,[{target:'Den@S2',playlist:{id:'SQ:8',title:'Evening & Friends'},logged:true}]);
});

test('a retained queue can be saved while a radio source and its playhead change',async()=>{
  const f=await fixture().ready(),p=await review(f);
  f.rows.get('Den@S2').track={uri:'x-sonosapi-stream:station',title:'Radio'};
  f.rows.get('Den@S2').volume=62;f.rows.get('Den@S2').transport_state='paused';
  f.queues.get('Den@S2').playing=null;
  assert.equal(await f.control.apply(p),true);assert.equal(f.writes().length,1);
  assert.equal(f.rows.get('Den@S2').volume,62);assert.equal(f.rows.get('Den@S2').track.title,'Radio');
});

test('duplicate titles create separate confirmed native identities only after a new read and review',async()=>{
  const f=await fixture().ready();await review(f,'Evening S2');assert.equal(await f.control.apply(),true);
  assert.equal(await f.control.review(),null);assert.equal(await f.control.apply(),false);
  await review(f,'Evening S2');assert.equal(await f.control.apply(),true);
  assert.deepEqual(f.catalogs.get('Den@S2').map(item=>item.id),['SQ:7','SQ:8','SQ:9']);
  assert.equal(f.writes().length,2);
});

test('changed queue revision, total or membership fences the POST',async()=>{
  for(const change of [f=>{f.queues.get('Den@S2').revision='new';},f=>{f.queues.get('Den@S2').total=4;},
    f=>{f.queues.get('Den@S2').total=0;f.queues.get('Den@S2').items=[];f.queues.get('Den@S2').playing=null;},
    f=>{f.zones[0].members.push('New room');}]) {
    const f=await fixture().ready(),p=await review(f);change(f);
    assert.equal(await f.control.apply(p),false);assert.equal(f.writes().length,0);
    assert.equal(f.controller.state.uncertain,false);assert.equal(f.control.state.result.status,'not_sent');
  }
});

test('editing, expiry, cancellation and selection retire old reviews',async()=>{
  for(const action of [f=>f.control.edit('title','Different'),f=>f.time(1000+Save.MAX_AGE),
    f=>f.control.cancelReview(),f=>f.controller.select('Den@S1'),f=>f.controller.invalidate(['Den@S2'])]) {
    const f=await fixture().ready(),p=await review(f);action(f);
    assert.equal(await f.control.apply(p),false);assert.equal(f.writes().length,0);
  }
});

test('closing during the fresh queue preflight prevents dispatch',async()=>{
  const f=await fixture().ready(),p=await review(f),gate=deferred();
  f.read((url,options,next)=>url.includes('/queue?')?gate.promise:next(url));
  const pending=f.control.apply(p);await settled(()=>f.calls.at(-1).url.includes('/queue?'));
  f.control.close();gate.resolve(copy(f.queues.get('Den@S2')));
  assert.equal(await pending,false);assert.equal(f.writes().length,0);assert.equal(f.controller.state.uncertain,false);
});

test('a read that finishes after a group change cannot populate the new group',async()=>{
  const f=await fixture().ready(),gate=deferred();f.control.open();
  f.read((url,options,next)=>url.includes('/queue?')?gate.promise:next(url));
  const pending=f.control.read();await settled(()=>f.calls.at(-1).url.includes('/queue?'));
  f.controller.select('Den@S1');gate.resolve(copy(f.queues.get('Den@S2')));
  assert.equal(await pending,false);assert.equal(f.control.state.target,'Den@S1');
  assert.equal(f.control.state.queue,null);assert.equal(f.control.state.loaded,false);
});

test('one sent write keeps the global lane occupied across close and reopen',async()=>{
  const f=await fixture().ready(),p=await review(f),gate=deferred();
  f.write(()=>gate.promise);const pending=f.control.apply(p);await settled(()=>f.writes().length===1);
  assert.equal(await f.control.apply(p),false);f.control.close();f.control.open();
  assert.equal(f.control.state.writing,true);assert.equal(await f.control.read(),false);
  assert.equal(await f.controller.command('pause'),false);
  gate.resolve({done:'Saved',zone:'Den',playlist:{id:'SQ:8',title:p.title},logged:true});
  assert.equal(await pending,true);assert.equal(f.writes().length,1);
  assert.equal(f.control.state.result.status,'accepted');assert.equal(f.control.state.result.target,p.target);
});

test('a confirmed save with a failed action log remains successful and warns against repeating it',async()=>{
  const f=await fixture().ready();await review(f);
  f.write((url,body,options,next)=>({...next(url,body),logged:false}));
  assert.equal(await f.control.apply(),true);assert.equal(f.controller.state.uncertain,false);
  assert.equal(f.control.state.result.logged,false);assert.match(f.control.state.result.detail,/Do not repeat/);
  assert.equal(f.saved[0].logged,false);assert.equal(f.writes().length,1);
});

test('incomplete, wrong-destination and mismatched-title receipts stay uncertain',async()=>{
  for(const transform of [v=>({...v,zone:'Elsewhere'}),v=>({...v,logged:undefined}),v=>({...v,done:''}),
    v=>({...v,playlist:{id:'FV:2/3',title:v.playlist.title}}),v=>({...v,playlist:{id:'SQ:8',title:'Different'}}),v=>null]) {
    const f=await fixture().ready();await review(f);
    f.write((url,body,options,next)=>transform(next(url,body)));
    assert.equal(await f.control.apply(),false);assert.equal(f.controller.state.uncertain,true);
    assert.equal(f.control.state.result.status,'uncertain');assert.equal(f.control.state.result.playlist,null);
    assert.equal(f.saved.length,0);assert.equal(f.writes().length,1);
  }
});

async function lostSave(f) {
  await review(f);f.write((url,body,options,next)=>{next(url,body);throw new Error('Reply lost after save');});
  assert.equal(await f.control.apply(),false);assert.equal(f.writes().length,1);assert.equal(f.controller.state.uncertain,true);
}

test('playback refresh and another household catalog cannot resolve a lost save',async()=>{
  const f=await fixture().ready();await lostSave(f);
  await f.controller.refresh(true);assert.equal(f.controller.state.uncertain,true);
  await f.controller.refreshStored('playlists',Save.decodeCatalog,{explicit:true,storeTarget:'Den@S1'});
  assert.equal(f.controller.state.uncertain,true);assert.equal(await f.control.read(),false);
  assert.equal(await f.control.apply(),false);assert.equal(f.writes().length,1);
});

test('original-household catalog recovery survives close and selection changes without confirming the lost write',async()=>{
  const f=await fixture().ready();await lostSave(f);f.control.close();f.controller.select('Den@S1');f.control.open();
  assert.equal(f.control.state.target,'Den@S1');assert.equal(f.control.state.result.target,'Den@S2');
  assert.equal(f.control.canCheckCatalog(),true);const catalog=await f.control.checkCatalog();
  assert.equal(f.calls.at(-1).url,'/sonos/playlists?zone=Den%40S2');assert.equal(catalog.length,2);
  assert.equal(f.controller.state.uncertain,false);assert.equal(f.control.state.catalogTarget,'Den@S2');
  assert.equal(f.control.state.catalogStatus,'ready');assert.equal(f.control.state.result.status,'uncertain');
  assert.match(f.control.state.message,/matching title does not prove/);assert.equal(f.control.state.loaded,false);
  assert.equal(f.control.canReview(),false);assert.equal(f.writes().length,1);assert.equal(f.saved.length,0);
});

test('failed, malformed or hidden catalog reads never clear uncertainty',async()=>{
  for(const mode of ['failure','malformed','hidden']) {
    const f=await fixture().ready();await lostSave(f);const gate=deferred();
    f.read((url,options,next)=>{
      if(!url.startsWith('/sonos/playlists?'))return next(url);
      if(mode==='failure')throw new Error('Catalog offline');
      if(mode==='malformed')return [{id:'SQ:8',title:'Mix'},{id:'SQ:8',title:'Duplicate id'}];
      return gate.promise;
    });
    const pending=f.control.checkCatalog();if(mode==='hidden'){f.control.suspend();gate.resolve(copy(f.catalogs.get('Den@S2')));}
    assert.equal(await pending,null);assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
  }
});

test('selection changes during original-catalog inspection do not change its recovery scope',async()=>{
  const f=await fixture().ready();await lostSave(f);const gate=deferred();
  f.read((url,options,next)=>url.startsWith('/sonos/playlists?')?gate.promise:next(url));
  const pending=f.control.checkCatalog();f.controller.select('Den@S1');gate.resolve(copy(f.catalogs.get('Den@S2')));
  assert.equal((await pending).length,2);assert.equal(f.control.state.catalogTarget,'Den@S2');
  assert.equal(f.controller.state.uncertain,false);assert.equal(f.writes().length,1);
});

test('hidden, disposed and demo controls cannot save or launch reads',async()=>{
  for(const mode of ['hidden','disposed','demo']) {
    const f=await fixture({demo:mode==='demo'}).ready();f.control.open();f.control.edit('title','Mix');
    if(mode==='hidden')f.control.suspend();if(mode==='disposed')f.control.dispose();const before=f.calls.length;
    assert.equal(await f.control.read(),false);assert.equal(await f.control.review(),null);assert.equal(await f.control.apply(),false);
    assert.equal(await f.control.checkCatalog(),null);assert.equal(f.calls.length,before);assert.equal(f.writes().length,0);
  }
});

test('presentation and saved-callback failures cannot replay a confirmed command',async()=>{
  const f=await fixture({changed:()=>{throw new Error('View failure');}}).ready();await review(f);
  f.after(()=>{throw new Error('Catalog view failure');});assert.equal(await f.control.apply(),true);
  assert.equal(f.controller.state.uncertain,false);assert.equal(f.control.state.result.status,'accepted');
  assert.match(f.control.state.message,/Refresh saved Sonos playlists/);assert.equal(f.writes().length,1);
});

test('real request transport loss preserves scoped uncertainty without a second POST',async()=>{
  const f=await fixture({network:raw=>C.request({fetcher:async(url,init)=>{
    const body=init.body===undefined?undefined:JSON.parse(init.body),value=await raw(url,body);
    if(body!==undefined)throw new TypeError('fetch rejected after submission');
    return new Response(JSON.stringify(value));
  }})}).ready();await review(f);assert.equal(await f.control.apply(),false);
  assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
  assert.equal((await f.control.checkCatalog()).length,2);assert.equal(f.controller.state.uncertain,false);
  assert.equal(f.writes().length,1);
});

test('native save and recovery catalog travel over real loopback JSON routes',async t=>{
  const server=http.createServer(async(req,res)=>{
    let raw='';for await(const part of req)raw+=part;
    try {const value=await f.raw(req.url,raw?JSON.parse(raw):undefined);res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(value));}
    catch(error){res.writeHead(500,{'content-type':'application/json'});res.end(JSON.stringify({detail:error.message}));}
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  t.after(()=>{server.closeAllConnections();return new Promise(resolve=>server.close(resolve));});
  const base='http://127.0.0.1:'+server.address().port;
  const f=fixture({network:()=>C.request({fetcher:(url,init)=>fetch(base+url,init)})});
  t.after(()=>f.control.dispose());await f.ready();await review(f,'Dvořák & friends');assert.equal(await f.control.apply(),true);
  assert.equal((await f.control.checkCatalog()).at(-1).title,'Dvořák & friends');
  assert.equal(f.writes().length,1);assert.equal(f.writes()[0].url,'/sonos/playlists/save');
});
