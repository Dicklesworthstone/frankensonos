'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Source = require('../crates/fsonos-api/src/remote/dashboard-source.js');
const ID = '0123456789ABCDEFGHIJKL';
const uri = kind => 'spotify:' + kind + ':' + ID;
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise,resolve,reject}; };
const settled = async predicate => {
  for (let i=0;i<200&&!predicate();i++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes=new Map();
  const doc={activeElement:null,createElement(tag) {
    const listeners=new Map();
    return {tag,children:[],open:false,hidden:false,disabled:false,textContent:'',value:'',attributes:{},
      setAttribute(k,v){this.attributes[k]=v;},append(...v){this.children.push(...v);},replaceChildren(...v){this.children=v;},
      addEventListener(k,fn){listeners.set(k,fn);},fire(k,e={}){return listeners.get(k)?.({target:this,preventDefault(){},...e});},
      showModal(){this.open=true;},close(){this.open=false;this.fire('close');},focus(){doc.activeElement=this;}};
  },getElementById(id) {if(!nodes.has(id))nodes.set(id,{...doc.createElement('div'),id});return nodes.get(id);}};
  return doc;
}

test('dashboard line-in controls read explicitly and render input identity before one reviewed play', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  const item = { player: 'RINCON_FIXTURE_INPUT', room: 'Record player', name: '<img src=x onerror=alert(1)> Turntable', icon: '1' };
  const catalog = { sources: [item], unavailable: [{ player: 'RINCON_FIXTURE_OFFLINE', room: 'Other input', detail: 'Player unavailable.' }] };
  f.read((url, options, read) => url.startsWith('/line-in?') ? copy(catalog) : read(url));
  f.write((url, body) => {
    assert.equal(url, '/line-in/play'); assert.deepEqual(body, { zone: 'Den@S2', source: item.player });
    return { done: 'Playing line-in', changed: true };
  });
  $('open-source').fire('click'); $('source-type').value = 'line_in'; $('source-type').fire('change');
  assert.equal($('source-line-in-field').hidden, false); assert.equal($('source-line-in-controls').hidden, false);
  assert.equal($('source-link-field').hidden, true); assert.equal($('source-mode-field').hidden, true);
  assert.equal($('source-mode').disabled, true); assert.equal($('source-review').disabled, true);
  assert.equal(f.calls.filter(call => call.url.startsWith('/line-in')).length, 0);
  $('source-line-in-refresh').fire('click'); await settled(() => f.source.state.lineInStatus === 'ready');
  assert.equal($('source-line-in').children.length, 2);
  assert.equal($('source-line-in').children[1].textContent, item.name + ' · ' + item.room);
  assert.equal($('source-line-in').children[1].value, item.player);
  assert.match($('source-line-in-help').textContent, /Other input: Player unavailable/);
  $('source-line-in').value = item.player; $('source-line-in').fire('change');
  assert.equal($('source-review').disabled, false); assert.equal(f.writes().length, 0);
  $('source-review').fire('click'); await settled(() => f.source.state.pending !== null);
  assert.match($('source-preview-source').textContent, /Line-in · <img/);
  assert.match($('source-preview-effect').textContent, /signal state are not reported/);
  assert.equal($('source-preview-queue').hidden, true); assert.equal($('source-confirm').disabled, false);
  $('source-confirm').fire('click'); await settled(() => f.source.state.result?.status === 'accepted');
  assert.equal(f.writes().length, 1); assert.equal(f.calls.filter(call => call.url.startsWith('/line-in?')).length, 3);
});
function fixture({demo=false,dom=false,network}={}) {
  const zones=['S2','S1'].map((household,i)=>({household,coordinator_room:'Den',members:i?['Den']:['Den','Side'],transport_state:'playing'}));
  const rows=new Map(zones.map(zone=>[C.key(zone),{zone,transport_state:'playing',volume:20,mute:false,
    track:{uri:'fixture:current',title:'Current song',queue_position:3,position_secs:20,duration_secs:300}}]));
  const queues=new Map(zones.map(zone=>[C.key(zone),{zone:zone.coordinator_room,total:12,revision:'queue:'+zone.household,playing:3,
    items:[{position:1,uri:'fixture:first',title:'First song'}]}]));
  const catalogs=new Map(['S1','S2'].map(home=>['Den@'+home,[{id:'SQ:7',title:'Evening & Friends '+home}]]));
  let time=1000,reader=null,writer=null,onComplete=null,source;
  const calls=[],completed=[],read=url=>{
    if(url==='/zones')return copy(zones);
    if(url.startsWith('/sonos/playlists?'))return copy(catalogs.get(new URLSearchParams(url.split('?')[1]).get('zone')));
    if(url.startsWith('/spotify/search?')){
      const query=new URLSearchParams(url.split('?')[1]);
      return {query:query.get('q'),kind:query.get('kind'),items:[{source_uri:uri(query.get('kind')),title:'Found on Spotify',subtitle:'Catalog creator'}],more:false};
    }
    const target=decodeURIComponent(url.split('/')[2]||'');
    if(url.endsWith('/state'))return copy(rows.get(target));
    if(url.endsWith('/queue?offset=0&limit=1'))return copy(queues.get(target));
    if(url==='/schedules')return [];
    throw new Error('Unexpected read '+url);
  };
  const write=(url,body)=>{
    if(url==='/play'||url==='/sonos/playlists/play')return {done:'Playback accepted',changed:true,notes:[{detail:'Volume capped.'}]};
    assert.equal(url,'/queue');const q=queues.get(body.zone),count=body.playlist?3:body.source_uri.includes(':track:')?1:3;
    const first=body.next&&q.playing!==null?q.playing+1:q.total+1;
    q.total+=count;q.revision+=':added';
    return {done:'Source queued',zone:q.zone,first,count,playing_queue:q.playing!==null};
  };
  const raw=async(url,body,options)=>{
    calls.push({url,body,options});
    if(body!==undefined)return writer?writer(url,body,options,write):write(url,body);
    return reader?reader(url,options,read):read(url);
  };
  const call=network?network(raw):raw;
  const controller=C.model({call,demo,now:()=>time,changed:()=>source?.observe()});
  const options={call,controller,now:()=>time,completed:async result=>{completed.push(result);if(onComplete)await onComplete(result);}};
  const doc=dom?documentFixture():null;
  source=dom?Source.panel({...options,document:doc}):Source.model(options);
  return {controller,source,zones,rows,queues,catalogs,calls,completed,doc,
    read(fn){reader=fn;},write(fn){writer=fn;},after(fn){onComplete=fn;},time(n){time=n;},
    writes:()=>calls.filter(row=>row.body!==undefined),
    async ready(){assert.equal(await controller.refresh(),true);controller.connection(true);return this;}};
}
async function review(f,kind='track',mode='play',title='') {
  f.source.open();f.source.edit('input',uri(kind));f.source.edit('title',title);f.source.edit('mode',mode);
  const p=await f.source.review();assert.ok(p);return p;
}

test('opening is lazy and source validation precedes every destination read',async()=>{
  const f=await fixture().ready(),before=f.calls.length;
  f.source.open();assert.equal(f.calls.length,before);assert.equal(f.source.canReview(),false);
  for(const value of ['not a URL','file:///tmp/audio.wav','spotify:track:short','https://name@audio.example/live']){
    f.source.edit('input',value);assert.equal(await f.source.review(),null);assert.equal(f.calls.length,before);
  }
  assert.equal(f.writes().length,0);
});

for(const kind of ['track','album','playlist','artist','episode','show'])test('Spotify '+kind+' playback uses one reviewed qualified command',async()=>{
  const f=await fixture().ready(),p=await review(f,kind,'play','  Display title  ');
  assert.ok(Object.isFrozen(p));assert.ok(Object.isFrozen(p.body));assert.ok(Object.isFrozen(p.before));assert.ok(Object.isFrozen(p.group));
  assert.equal(f.writes().length,0);assert.equal(await f.source.apply(p),true);assert.equal(await f.source.apply(p),false);
  assert.deepEqual(f.writes().map(({url,body})=>({url,body})),[{url:'/play',body:{zone:'Den@S2',source_uri:uri(kind),title:'Display title'}}]);
  assert.match(f.source.state.result.detail,/Volume capped/);assert.equal(f.source.state.result.status,'accepted');
  assert.deepEqual(f.completed,[{target:'Den@S2',mode:'play'}]);assert.equal(f.controller.state.uncertain,false);
});

test('show reviews explain the bounded episode selection for every mode',async()=>{
  for(const mode of ['play','next','end']){
    const f=await fixture().ready(),p=await review(f,'show',mode),effect=Source.effect(p);
    assert.equal(p.item.container,true);assert.equal(p.before.queue.total,12);
    assert.match(effect,/up to 10 available episodes/);assert.match(effect,/Spotify’s order/);
    assert.match(effect,/Requires Spotify sign-in/);
    assert.match(effect,mode==='play'?/Replace the current queue/:mode==='next'?/without restarting/:/without starting or resuming/);
    assert.equal(f.writes().length,0);
  }
});

test('artist reviews explain the bounded selection and each playback effect',async()=>{
  for(const mode of ['play','next','end']){
    const f=await fixture().ready(),p=await review(f,'artist',mode),effect=Source.effect(p);
    assert.match(effect,/up to 10 tracks/);assert.match(effect,/Requires Spotify sign-in/);
    assert.match(effect,mode==='play'?/Replace the current queue/:mode==='next'?/without restarting/:/without starting or resuming/);
    assert.equal(f.writes().length,0);
  }
});

test('all six Spotify kinds can queue next or append without a play or resume command',async()=>{
  for(const kind of ['track','album','playlist','artist','episode','show'])for(const mode of ['next','end']){
    const f=await fixture().ready(),p=await review(f,kind,mode);
    assert.equal(await f.source.apply(p),true);
    assert.deepEqual(f.writes().map(({url,body})=>({url,body})),[{url:'/queue',body:{zone:'Den@S2',source_uri:uri(kind),next:mode==='next'}}]);
    assert.match(f.source.state.result.detail,new RegExp('position '+(mode==='next'?4:13)));
    assert.equal(f.completed[0].mode,mode);
  }
});

test('signed HTTP stream URLs are preserved and require Play now',async()=>{
  const f=await fixture().ready(),value='https://audio.example/Live%2fTrack.mp3?Signature=a%2Bb&part=1';
  f.source.open();f.source.edit('input',value);f.source.edit('mode','next');
  const before=f.calls.length;assert.equal(await f.source.review(),null);assert.equal(f.calls.length,before);
  f.source.edit('mode','play');const p=await f.source.review();assert.ok(p);
  assert.equal(p.body.source_uri,value);assert.equal(await f.source.apply(p),true);
  assert.deepEqual(f.writes()[0].body,{zone:'Den@S2',source_uri:value});assert.ok(f.calls.every(row=>row.url.startsWith('/')));
});

test('replacing album, playlist, artist or show content also reads and binds the old queue revision',async()=>{
  for(const kind of ['album','playlist','artist','show']){
    const f=await fixture().ready(),p=await review(f,kind);
    assert.equal(p.before.queue.total,12);assert.match(Source.effect(p),/Replace the current queue/);
    f.queues.get('Den@S2').revision='queue:changed';
    assert.equal(await f.source.apply(p),false);assert.equal(f.writes().length,0);assert.equal(f.controller.state.uncertain,false);
  }
});

test('track playback does not require a library, Spotify lookup, DJ status or queue browse',async()=>{
  const f=await fixture().ready(),before=f.calls.length;await review(f);assert.equal(await f.source.apply(),true);
  assert.ok(f.calls.slice(before).every(row=>['/zones','/zones/Den%40S2/state','/play'].includes(row.url)));
});

test('Play now binds source, occurrence and transport while ignoring progress, volume and labels',async()=>{
  for(const change of [row=>{row.track.uri='fixture:other';},row=>{row.track.queue_position=4;},row=>{row.transport_state='paused';}]){
    const f=await fixture().ready();await review(f);change(f.rows.get('Den@S2'));
    assert.equal(await f.source.apply(),false);assert.equal(f.writes().length,0);
  }
  const f=await fixture().ready();await review(f);const row=f.rows.get('Den@S2');
  row.track.position_secs=31;row.track.title='New label';row.volume=25;row.mute=true;
  assert.equal(await f.source.apply(),true);
});

test('append tolerates a natural track change but Queue next binds the playing occurrence',async()=>{
  const append=await fixture().ready();await review(append,'track','end');
  append.rows.get('Den@S2').track.uri='fixture:next';append.rows.get('Den@S2').track.queue_position=4;append.queues.get('Den@S2').playing=4;
  assert.equal(await append.source.apply(),true);
  const next=await fixture().ready();await review(next,'track','next');next.queues.get('Den@S2').playing=4;
  assert.equal(await next.source.apply(),false);assert.equal(next.writes().length,0);
});

test('Queue next on an inactive queue reports append semantics without starting playback',async()=>{
  const f=await fixture().ready();f.queues.get('Den@S2').playing=null;const p=await review(f,'track','next');
  assert.match(Source.effect(p),/Append/);assert.equal(await f.source.apply(),true);
  assert.match(f.source.state.result.detail,/not using its queue/);assert.match(f.source.state.result.detail,/position 13/);
});

test('queue revision, length and group membership changes prevent queue writes',async()=>{
  for(const change of [f=>{f.queues.get('Den@S2').revision='new';},f=>{f.queues.get('Den@S2').total++;},f=>{f.zones[0].members.push('New room');}]){
    const f=await fixture().ready();await review(f,'album','end');change(f);
    assert.equal(await f.source.apply(),false);assert.equal(f.writes().length,0);
  }
});

test('changed group selection retires consent and preserves the source draft',async()=>{
  const f=await fixture().ready(),p=await review(f,'track','next','Keep title');
  f.controller.select('Den@S1');assert.equal(f.source.state.pending,null);assert.equal(await f.source.apply(p),false);
  assert.equal(f.source.state.input,uri('track'));assert.equal(f.source.state.title,'Keep title');
  const next=await f.source.review();assert.equal(next.body.zone,'Den@S1');assert.equal(await f.source.apply(next),true);
});

test('editing, cancelling and expiry invalidate captured confirmations without discarding the draft',async()=>{
  const f=await fixture().ready(),p=await review(f);
  f.source.edit('title','Edited');assert.equal(await f.source.apply(p),false);
  const next=await f.source.review();f.source.cancelReview();assert.equal(await f.source.apply(next),false);
  const old=await f.source.review();f.time(1000+Source.MAX_AGE);f.source.observe();
  assert.equal(f.source.state.pending,null);assert.equal(await f.source.apply(old),false);assert.equal(f.source.state.input,uri('track'));
});

test('a slow preflight cannot outlive its review and every unsent lifecycle exit cancels the read',async()=>{
  for(const action of ['close','suspend','dispose','expire']){
    const f=await fixture().ready();await review(f);const gate=deferred();f.read(()=>gate.promise);
    const work=f.source.apply();await settled(()=>f.controller.state.busy);const signal=f.calls.at(-1).options.signal;
    if(action==='expire')f.time(1000+Source.MAX_AGE);else{f.source[action]();assert.equal(signal.aborted,true);}
    gate.resolve(copy(f.zones));assert.equal(await work,false);assert.equal(f.writes().length,0);assert.equal(f.controller.state.uncertain,false);
  }
});

test('the source preflight owns the common playback and stored-routine command lane',async()=>{
  const f=await fixture().ready(),p=await review(f),gate=deferred();
  f.read((url,options,normal)=>url==='/zones'?gate.promise:normal(url));
  const work=f.source.apply(p);await settled(()=>f.controller.state.busy);
  assert.equal(await f.source.apply(p),false);assert.equal(await f.controller.command('pause'),false);
  assert.equal(await f.controller.mutate(()=>assert.fail('stored action must not run'),{stored:'schedules'}),false);
  assert.equal(f.source.edit('input',uri('album')),false);
  gate.resolve(copy(f.zones));assert.equal(await work,true);assert.equal(f.writes().length,1);
});

test('an existing stored operation cannot be bypassed by source confirmation',async()=>{
  const f=await fixture().ready();await review(f);const gate=deferred();
  const work=f.controller.mutate(async()=>{await gate.promise;return{done:'Stored',changed:true};},{stored:'schedules'});
  assert.equal(await f.source.apply(),false);gate.resolve();await work;assert.equal(f.writes().length,0);
});

test('editing a pending read aborts it and stale completion cannot replace newer consent',async()=>{
  const f=await fixture().ready();f.source.open();f.source.edit('input',uri('track'));const gate=deferred();
  f.read(()=>gate.promise);const work=f.source.review();await settled(()=>f.source.state.loading);const signal=f.calls.at(-1).options.signal;
  f.source.edit('title','New title');assert.equal(signal.aborted,true);f.read(null);
  const p=await f.source.review();assert.equal(p.title,'New title');
  gate.resolve(copy(f.zones));assert.equal(await work,null);assert.equal(f.source.state.pending,p);
});

test('closing or hiding after submission never cancels or retries and retains the receipt',async()=>{
  for(const action of ['close','suspend']){
    const f=await fixture().ready();await review(f);const gate=deferred();f.write(()=>gate.promise);
    const work=f.source.apply();await settled(()=>f.writes().length===1);assert.equal(f.writes()[0].options?.signal,undefined);
    f.source[action]();if(action==='suspend')f.controller.suspend();
    gate.resolve({done:'Playback accepted',changed:true});assert.equal(await work,true);
    assert.equal(f.source.state.open,false);assert.equal(f.source.state.result.status,'accepted');assert.equal(f.writes().length,1);
  }
});

test('lost replies, policy refusals and malformed play receipts are never retried',async()=>{
  for(const value of [new Error('Reply lost'),Object.assign(new Error('Denied'),{hint:'Ask the owner'}),{}, {done:'ok',changed:true,notes:[{}]}]){
    const f=await fixture().ready(),p=await review(f);
    f.write(()=>{if(value instanceof Error)throw value;return value;});
    assert.equal(await f.source.apply(),false);assert.equal(f.controller.state.uncertain,true);
    assert.equal(f.source.state.result.status,'uncertain');assert.equal(await f.source.apply(p),false);
    await f.controller.refreshStored('schedules',x=>x,{explicit:true});assert.equal(f.controller.state.uncertain,true);
    assert.equal(f.writes().length,1);
  }
});

test('malformed queue receipts cannot release the dashboard for another command',async()=>{
  for(const patch of [{zone:'Other'}, {first:0}, {count:0}, {first:0xffffffff,count:2}, {playing_queue:null}, {done:''}]){
    const f=await fixture().ready();await review(f,'track','end');
    f.write((url,body,options,normal)=>({...normal(url,body),...patch}));
    assert.equal(await f.source.apply(),false);assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
  }
});

test('observer refresh failure cannot turn an accepted native action into uncertainty',async()=>{
  const f=await fixture().ready();await review(f);f.after(()=>{throw new Error('status unavailable');});
  assert.equal(await f.source.apply(),true);assert.equal(f.source.state.result.status,'accepted');
  assert.equal(f.controller.state.uncertain,false);assert.match(f.source.state.message,/Refresh the queue/);assert.equal(f.writes().length,1);
});

test('source handoff cannot select a destination, overwrite a draft or send music',async()=>{
  const f=await fixture().ready();
  assert.equal(f.source.offer({source_uri:uri('track'),zone:'Den@S1'}),false);
  assert.equal(f.source.offer({source_uri:uri('track'),title:'Offered'}),true);assert.equal(f.source.state.pending,null);
  assert.equal(f.source.offer({source_uri:uri('album')}),false);assert.equal(f.source.state.input,uri('track'));
  f.source.clearDraft();assert.equal(f.source.offer({source_uri:uri('album')}),true);assert.equal(f.writes().length,0);
});

test('demo and disconnected playback cannot review or dispatch sources',async()=>{
  const demo=await fixture({demo:true}).ready();demo.source.open();demo.source.edit('input',uri('track'));
  assert.equal(await demo.source.review(),null);assert.equal(demo.writes().length,0);
  const offline=await fixture().ready(),p=await review(offline);offline.controller.connection(false);
  assert.equal(await offline.source.apply(p),false);assert.equal(offline.writes().length,0);
});

test('panel handlers retain typed values, show exact effects and use text-only source titles',async()=>{
  const f=await fixture({dom:true}).ready(),$=id=>f.doc.getElementById(id);
  $('open-source').focus();$('open-source').fire('click');assert.equal($('source-dialog').open,true);
  $('source-input').value=uri('album');$('source-input').fire('input');
  $('source-title').value='<script>Display only</script>';$('source-title').fire('input');
  $('source-review').focus();$('source-review').fire('click');await settled(()=>!!f.source.state.pending);
  assert.equal(f.doc.activeElement.id,'source-preview');assert.match($('source-preview-title').textContent,/<script>Display only/);
  assert.match($('source-preview-effect').textContent,/Replace the current queue/);assert.match($('source-preview-target').textContent,/S2/);
  $('source-back').fire('click');assert.equal(f.doc.activeElement.id,'source-review');assert.equal($('source-input').value,uri('album'));
  $('source-review').fire('click');await settled(()=>!!f.source.state.pending);
  $('source-confirm').fire('click');await settled(()=>!!f.source.state.result);
  assert.equal($('source-result').hidden,false);assert.match($('source-result-detail').textContent,/Volume capped/);
  $('close-source').fire('click');assert.equal(f.doc.activeElement.id,'open-source');
});

test('shared source helpers load in a browser bundle without full-remote globals',()=>{
  const context=vm.createContext({});
  vm.runInContext(fs.readFileSync(require.resolve('../crates/fsonos-api/src/remote/source-player.js'),'utf8'),context);
  assert.equal(vm.runInContext('typeof SourcePlayerRemote.prepare',context),'function');
});

test('real HTTP adapter sends the reviewed source once and validates the native queue receipt',async t=>{
  let raw,origin;const received=[];
  const server=http.createServer(async(request,response)=>{
    const chunks=[];for await(const chunk of request)chunks.push(chunk);
    const body=chunks.length?JSON.parse(Buffer.concat(chunks).toString('utf8')):undefined;
    received.push({url:request.url,method:request.method,body,contentType:request.headers['content-type']});
    try{const value=await raw(request.url,body);response.writeHead(200,{'Content-Type':'application/json'});response.end(JSON.stringify(value));}
    catch(error){response.writeHead(500,{'Content-Type':'application/json'});response.end(JSON.stringify({detail:error.message}));}
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));origin='http://127.0.0.1:'+server.address().port;
  t.after(()=>new Promise(resolve=>server.close(resolve)));
  const f=await fixture({network(fn){raw=fn;return C.request({fetcher:(url,options)=>fetch(origin+url,options)});}}).ready();
  await review(f,'artist','end');assert.equal(await f.source.apply(),true);
  const writes=received.filter(row=>row.method!=='GET');assert.equal(writes.length,1);
  assert.deepEqual(writes[0],{url:'/queue',method:'POST',body:{zone:'Den@S2',source_uri:uri('artist'),next:false},contentType:'application/json'});
  assert.equal(f.source.state.result.status,'accepted');
});

async function reviewSaved(f,mode='play') {
  f.source.open();f.source.edit('sourceType','playlist');assert.equal(await f.source.loadPlaylists(),true);
  assert.equal(f.source.edit('playlist','SQ:7'),true);f.source.edit('mode',mode);
  const p=await f.source.review();assert.ok(p);return p;
}

for(const home of ['S1','S2'])for(const mode of ['play','next','end'])test('dashboard saved playlist '+mode+' stays within '+home,async()=>{
  const f=await fixture().ready();f.controller.select('Den@'+home);const p=await reviewSaved(f,mode);
  assert.equal(p.title,'Evening & Friends '+home);assert.equal(p.item.kind,'sonos_playlist');assert.equal(p.before.queue.total,12);
  assert.equal(f.writes().length,0);assert.equal(await f.source.apply(p),true);assert.equal(await f.source.apply(p),false);
  assert.deepEqual(f.writes().map(({url,body})=>({url,body})),[{url:mode==='play'?'/sonos/playlists/play':'/queue',
    body:{zone:'Den@'+home,playlist:'SQ:7',...(mode==='play'?{}:{next:mode==='next'})}}]);
  const catalogs=f.calls.filter(({url})=>url.startsWith('/sonos/playlists?'));
  assert.equal(catalogs.length,3);assert.ok(catalogs.every(({url})=>url==='/sonos/playlists?zone=Den%40'+home));
});

test('dashboard selection change cannot carry the same saved id between households',async()=>{
  const f=await fixture().ready(),p=await reviewSaved(f);f.controller.select('Den@S1');
  assert.equal(f.source.state.playlist,'');assert.deepEqual(f.source.state.playlists,[]);
  assert.equal(await f.source.apply(p),false);assert.equal(await f.source.review(),null);assert.equal(f.source.edit('playlist','SQ:7'),false);
  assert.equal(await f.source.loadPlaylists(),true);assert.equal(f.source.state.playlist,'');
  f.source.edit('playlist','SQ:7');const next=await f.source.review();assert.equal(next.title,'Evening & Friends S1');
  assert.equal(await f.source.apply(next),true);assert.equal(f.writes()[0].body.zone,'Den@S1');
});

test('dashboard saved catalog changes or permission errors invalidate review without a write',async()=>{
  for(const catalog of [[],[{id:'SQ:7',title:'Renamed'}]]){
    const f=await fixture().ready(),p=await reviewSaved(f);f.catalogs.set('Den@S2',catalog);
    assert.equal(await f.source.apply(p),false);assert.equal(f.writes().length,0);assert.equal(f.source.state.result.status,'not_sent');
  }
  const f=await fixture().ready();await reviewSaved(f);f.read((url,options,normal)=>{
    if(url.startsWith('/sonos/playlists?'))throw Object.assign(new Error('Not allowed'),{hint:'Ask the owner to allow listing favorites.'});
    return normal(url);
  });
  assert.equal(await f.source.loadPlaylists(),false);assert.equal(f.source.state.playlistStatus,'error');
  assert.match(f.source.state.message,/Ask the owner/);assert.equal(f.source.state.playlist,'');assert.equal(f.source.state.pending,null);
});

test('late dashboard playlist reads are aborted and cannot revive a prior destination',async()=>{
  const f=await fixture().ready();await reviewSaved(f);const gate=deferred();
  f.read((url,options,normal)=>url.startsWith('/sonos/playlists?')?gate.promise:normal(url));
  const work=f.source.loadPlaylists();await settled(()=>f.source.state.loading);
  const signal=f.calls.at(-1).options.signal;f.controller.select('Den@S1');assert.equal(signal.aborted,true);
  gate.resolve(f.catalogs.get('Den@S2'));assert.equal(await work,false);
  assert.deepEqual(f.source.state.playlists,[]);assert.equal(f.source.state.playlist,'');assert.equal(f.writes().length,0);
});

test('native playlist replacement preserves queue and playback preflight guards',async()=>{
  for(const change of [f=>{f.queues.get('Den@S2').revision='changed';},f=>{f.rows.get('Den@S2').track.uri='sample:other';},
    f=>{f.zones[0].members.push('Other');}]){
    const f=await fixture().ready(),p=await reviewSaved(f);change(f);
    assert.equal(await f.source.apply(p),false);assert.equal(f.writes().length,0);
  }
});

test('saved playlist confirmation shares the command lane and never retries an ambiguous reply',async()=>{
  const f=await fixture().ready(),p=await reviewSaved(f),gate=deferred();f.write(()=>gate.promise);
  const work=f.source.apply(p);await settled(()=>f.writes().length===1);
  assert.equal(await f.source.apply(p),false);assert.equal(await f.controller.command('pause'),false);
  f.source.close();gate.reject(new Error('Reply lost'));assert.equal(await work,false);
  f.source.open();assert.equal(await f.source.apply(p),false);assert.equal(f.writes().length,1);
  assert.equal(f.source.state.result.status,'uncertain');assert.equal(f.writes()[0].url,'/sonos/playlists/play');
});

test('dashboard playlist picker uses text labels and an explicit review for the selected group',async()=>{
  const f=await fixture({dom:true}).ready(),$=id=>f.doc.getElementById(id),title='<script>Evening & Friends</script>';
  f.catalogs.set('Den@S2',[{id:'SQ:7',title}]);$('open-source').fire('click');
  $('source-type').value='playlist';$('source-type').fire('change');await settled(()=>f.source.state.playlistStatus==='ready');
  assert.equal($('source-link-field').hidden,true);assert.equal($('source-playlist-field').hidden,false);
  assert.equal($('source-playlist').children[1].textContent,title);assert.equal($('source-playlist').children[1].tag,'option');
  $('source-playlist').value='SQ:7';$('source-playlist').fire('change');$('source-review').fire('click');
  await settled(()=>!!f.source.state.pending);assert.match($('source-preview-title').textContent,/<script>Evening & Friends/);
  assert.match($('source-preview-effect').textContent,/saved Sonos playlist/);assert.match($('source-preview-target').textContent,/S2/);
  assert.equal(f.writes().length,0);$('source-confirm').fire('click');await settled(()=>!!f.source.state.result);
  assert.deepEqual(f.writes()[0].body,{zone:'Den@S2',playlist:'SQ:7'});
});

test('dashboard playlist catalog uses an explicit encoded group over real HTTP',async t=>{
  let raw,origin;const received=[];
  const server=http.createServer(async(request,response)=>{
    const chunks=[];for await(const chunk of request)chunks.push(chunk);
    const body=chunks.length?JSON.parse(Buffer.concat(chunks).toString('utf8')):undefined;
    received.push({url:request.url,method:request.method,body});
    try{const value=await raw(request.url,body);response.writeHead(200,{'Content-Type':'application/json'});response.end(JSON.stringify(value));}
    catch(error){response.writeHead(500,{'Content-Type':'application/json'});response.end(JSON.stringify({detail:error.message}));}
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));origin='http://127.0.0.1:'+server.address().port;
  t.after(()=>{server.closeAllConnections();server.close();});
  const f=await fixture({network(fn){raw=fn;return C.request({fetcher:(url,options)=>fetch(origin+url,options)});}}).ready();
  await reviewSaved(f,'next');assert.equal(await f.source.apply(),true);
  assert.deepEqual(received.filter(row=>row.method==='POST'),[{url:'/queue',method:'POST',body:{zone:'Den@S2',playlist:'SQ:7',next:true}}]);
  assert.equal(received.filter(row=>row.url==='/sonos/playlists?zone=Den%40S2').length,3);
});


test('saved playlist expansion gets the long command deadline without a retry',async()=>{
  let expire,budget,calls=0,cancelled=false;
  const request=C.request({fetcher:async()=>{calls++;return{ok:true,text:()=>new Promise(()=>{})};},
    schedule:(fn,ms)=>{expire=fn;budget=ms;return 1;},cancel:()=>{cancelled=true;}});
  const waiting=request('/sonos/playlists/play',{zone:'Den@S2',playlist:'SQ:7'});
  assert.equal(budget,600000);expire();await assert.rejects(waiting,/No command confirmation/);
  assert.equal(calls,1);assert.equal(cancelled,true);
});

for(const kind of ['track','album','artist','playlist','show','episode'])for(const mode of ['play','next','end']){
  test(`dashboard Spotify catalog ${kind} ${mode} keeps destination and action until explicit review`,async()=>{
    const f=await fixture().ready();f.source.open();f.source.edit('mode',mode);f.source.edit('sourceType','catalog');
    const before=f.calls.length;f.source.catalog.edit('query','  Find & hear?  ');f.source.catalog.edit('kind',kind);
    assert.equal(f.calls.length,before);assert.equal(await f.source.catalog.search(),true);
    const request=new URL(f.calls.at(-1).url,'http://fixture.invalid');
    assert.equal(request.pathname,'/spotify/search');assert.deepEqual(Object.fromEntries(request.searchParams),{q:'Find & hear?',kind,limit:'10'});
    assert.equal(f.source.canReview(),false);const items=f.source.state.catalog.items;
    assert.equal(f.source.chooseCatalog(items[0],items),true);assert.equal(f.source.state.mode,mode);
    assert.equal(f.source.state.target,'Den@S2');assert.equal(f.source.state.sourceType,'link');
    assert.equal(f.source.state.pending,null);assert.equal(f.writes().length,0);
    assert.equal(f.source.chooseCatalog(items[0],items),false);
    const review=await f.source.review();assert.ok(review);assert.equal(f.writes().length,0);
    assert.equal(await f.source.apply(review),true);assert.equal(await f.source.apply(review),false);
    assert.deepEqual(f.writes().map(({url,body})=>({url,body})),[{url:mode==='play'?'/play':'/queue',body:{
      zone:'Den@S2',source_uri:uri(kind),title:'Found on Spotify',...(mode==='play'?{}:{next:mode==='next'}),
    }}]);
  });
}

test('dashboard catalog can search before room selection and still requires a destination review',async()=>{
  const f=fixture();f.source.open();f.source.edit('sourceType','catalog');f.source.catalog.edit('query','Find music');
  assert.equal(await f.source.catalog.search(),true);assert.equal(f.source.chooseCatalog(f.source.state.catalog.items[0]),true);
  assert.equal(f.source.state.target,'');assert.equal(f.source.canReview(),false);assert.equal(f.writes().length,0);
  await f.ready();assert.ok(await f.source.review());assert.equal(f.writes().length,0);
});

test('dashboard catalog stale responses cannot survive cancellation, close, edits or changed rooms',async()=>{
  for(const retire of [f=>f.source.catalog.cancel(),f=>f.source.catalog.edit('kind','show'),
    f=>f.controller.select('Den@S1'),f=>f.source.close(),f=>f.source.suspend(),f=>f.source.dispose()]){
    const f=await fixture().ready();f.source.open();f.source.edit('sourceType','catalog');f.source.catalog.edit('query','Old search');
    const gate=deferred();let signal;
    f.read((url,options,normal)=>{if(url.startsWith('/spotify/search?')){signal=options.signal;return gate.promise;}return normal(url);});
    const work=f.source.catalog.search();assert.equal(f.source.state.catalog.status,'loading');
    retire(f);assert.equal(signal.aborted,true);
    gate.resolve({query:'Old search',kind:'track',items:[{source_uri:uri('track'),title:'Late result'}],more:false});
    assert.equal(await work,false);assert.deepEqual(f.source.state.catalog.items,[]);
    assert.equal(f.source.state.input,'');assert.equal(f.source.state.pending,null);assert.equal(f.writes().length,0);
  }
});

test('dashboard selected catalog source still refuses a room change after review',async()=>{
  const f=await fixture().ready();f.source.open();f.source.edit('sourceType','catalog');f.source.catalog.edit('query','Find music');
  await f.source.catalog.search();f.source.chooseCatalog(f.source.state.catalog.items[0]);const pending=await f.source.review();
  f.controller.select('Den@S1');assert.equal(await f.source.apply(pending),false);assert.equal(f.writes().length,0);
  assert.equal(f.source.state.input,uri('track'));assert.equal((await f.source.review()).target,'Den@S1');
});

test('dashboard catalog reports auth failures and empty results without a playback request',async()=>{
  const f=await fixture().ready();f.source.open();f.source.edit('sourceType','catalog');f.source.catalog.edit('query','Find music');
  f.read((url,options,normal)=>{if(url.startsWith('/spotify/search?'))throw Object.assign(new Error('Spotify sign-in required'),{hint:'Connect Spotify in music settings.'});return normal(url);});
  assert.equal(await f.source.catalog.search(),false);assert.equal(f.source.state.catalog.status,'error');
  assert.match(f.source.state.catalog.message,/Connect Spotify/);assert.equal(f.writes().length,0);
  f.read((url,options,normal)=>url.startsWith('/spotify/search?')?{query:'Find music',kind:'track',items:[],more:false}:normal(url));
  assert.equal(await f.source.catalog.search(),true);assert.equal(f.source.state.catalog.status,'ready');
  assert.match(f.source.state.catalog.message,/No matching Spotify results/);
});

test('dashboard catalog DOM submits once and treats provider metadata only as text',async()=>{
  const f=await fixture({dom:true}).ready(),$=id=>f.doc.getElementById(id),title='<script>Catalog title</script>'+'🎵'.repeat(300);
  f.read((url,options,normal)=>url.startsWith('/spotify/search?')?{query:'Find music',kind:'track',
    items:[{source_uri:uri('track'),title,subtitle:'<img src="https://external.invalid/art">'}],more:true}:normal(url));
  $('open-source').fire('click');$('source-type').value='catalog';$('source-type').fire('change');
  $('source-catalog-query').value='Find music';$('source-catalog-query').fire('input');
  assert.equal(f.calls.filter(({url})=>url.startsWith('/spotify/search?')).length,0);
  $('source-catalog-form').fire('submit');await settled(()=>f.source.state.catalog.status==='ready');
  const row=$('source-catalog-items').children[0];assert.equal(row.children[0].textContent,title);
  assert.equal(row.children[1].tag,'span');assert.match(row.children[1].textContent,/<img/);
  assert.equal($('source-review').disabled,true);row.children[2].fire('click');
  assert.equal(f.source.state.sourceType,'link');assert.equal([...f.source.state.title].length,256);
  assert.equal(f.source.state.pending,null);assert.equal(f.writes().length,0);
});
