'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-live.js');
const F = require('../crates/fsonos-api/src/remote/event-feed.js');
const script = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/dashboard.js'), 'utf8');
const begin = script.indexOf('  live = DashboardLive.create({'), end = script.indexOf('  function suspend() {', begin);
assert.ok(begin >= 0 && end > begin, 'exercise the actual page wiring, not a duplicate');
const wiring = script.slice(begin, end);
const settle = async () => { for (let n = 0; n < 100; n++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return { promise, resolve, reject }; }
function clock() {
  let now=0, id=0; const timers=new Map();
  return { now:()=>now, schedule(fn,ms) { timers.set(++id,{fn,at:now+ms});return id; }, cancel(id) {timers.delete(id);},
    get size(){return timers.size;}, async tick(ms) { const end=now+ms;await settle();for(let n=0;;n++) {
      assert.ok(n<10000,'no timer storm');const next=[...timers].filter(([,t])=>t.at<=end).sort((a,b)=>a[1].at-b[1].at)[0];
      if(!next)break;timers.delete(next[0]);now=next[1].at;next[1].fn();await settle();
    }now=end;await settle();} };
}
function setup() {
  const t=clock(), requests=[], streams=[], data={title:'First song',state:'playing'}, gate={until:0};
  const zone={household:'S2',coordinator_room:'Den',members:['Den'],transport_state:'playing'};
  let override=null, invalidations=0, histories=0;
  const playback=()=>({zone:{...zone},transport_state:data.state,volume:20,mute:false,
    track:{uri:'fixture:one',title:data.title,position_secs:20,duration_secs:600,queue_position:1}});
  const fetcher=async(url,init)=>{
    requests.push({url,init,at:t.now()});
    if(url==='/events') {
      let source;const record={source:null,aborted:false};
      const body=new ReadableStream({start(c){source=c;},cancel(){record.aborted=true;}});
      record.source=source;streams.push(record);source.enqueue(new TextEncoder().encode(': heartbeat\n\n'));
      return new Response(body,{headers:{'content-type':'text/event-stream'}});
    }
    if(override){const value=await override(url,init);if(value!==undefined)return value;}
    const value=init.method==='POST'?{done:'Accepted',changed:true}:url==='/zones'?[zone]:url.endsWith('/state')?playback()
      :url.endsWith('/dj')?{zone:'Den',running:false,now:null,next:[],steering:{source:'none'}}
      :url==='/dj/moods'?{moods:[{name:'focus',constraints:{}}]}:[];
    return new Response(JSON.stringify(value),{headers:{'content-type':'application/json'}});
  };
  const request=C.request({fetcher,schedule:t.schedule,cancel:t.cancel});
  const controller=C.model({call:request,now:t.now});
  const session=S.model({call:request,controller,now:t.now});
  const context=vm.createContext({controller,session,request,active:true,catalogsDirty:true,historyVersion:0,demo:false,
    workspace:{invalidate(){invalidations++;}},history:async()=>{histories++;},
    RemoteEventFeed:{open:(url)=>F.open(url,{fetcher,schedule:t.schedule,cancel:t.cancel,now:t.now,random:()=>0,gate})},
    DashboardLive:{create:options=>L.create({...options,schedule:t.schedule,cancel:t.cancel,now:t.now})},
  });
  vm.runInContext(wiring,context);
  const live=context.live;
  return {t,requests,streams,data,playback,controller,session,live,request,context,
    override(fn){override=fn;},get histories(){return histories;},get invalidations(){return invalidations;},
    frame(text,index=streams.length-1){streams[index].source.enqueue(new TextEncoder().encode(text));},
    async ready(){live.resume();await t.tick(350);assert.equal(controller.state.connected,true);},
    snapshotCount:()=>requests.filter(r=>r.url==='/zones').length,
    writes:()=>requests.filter(r=>r.init.method==='POST'),
    close(){live.dispose();request.cancelReads();},
  };
}

test('actual page binding uses the common heartbeat feed and does not poll a quiet house',async()=>{
  const h=setup();await h.ready();const before=h.requests.length;
  for(let i=0;i<8;i++){await h.t.tick(15000);h.frame(': heartbeat\n\n');await settle();}
  assert.equal(h.snapshotCount(),1);assert.equal(h.requests.length,before);assert.equal(h.controller.canControl(),true);
  assert.equal(h.histories,1);assert.equal(h.writes().length,0);h.close();assert.equal(h.t.size,0);
});

test('silent body stalls make the page stale, then resume through the same feed and a fresh snapshot',async()=>{
  const h=setup();await h.ready();h.frame('id: 9007199254740993\nevent: zone.state\ndata: {}\n\n');await h.t.tick(350);
  await h.t.tick(44650);assert.equal(h.controller.state.connected,false);assert.equal(h.controller.selected().fresh,false);
  assert.equal(h.streams[0].aborted,true);h.data.title='After reconnection';await h.t.tick(1350);
  assert.equal(h.controller.state.connected,true);assert.equal(h.controller.selected().state.track.title,'After reconnection');
  const events=h.requests.filter(r=>r.url==='/events');assert.equal(events[1].init.headers['Last-Event-ID'],'9007199254740993');
  assert.equal(h.writes().length,0);h.close();
});

test('an isolated reset rereads the house and clears pending reviews before another event',async()=>{
  const h=setup();await h.ready();h.session.review('start');assert.ok(h.session.state.pending);
  h.data.title='New journal';h.frame('event: events.reset\ndata: {"oldest":1}\n\n');await settle();
  assert.equal(h.session.state.pending,null);assert.equal(h.controller.selected().fresh,false);
  await h.t.tick(350);assert.equal(h.controller.selected().state.track.title,'New journal');assert.equal(h.snapshotCount(),2);
  h.close();
});

test('normal deltas preserve an immutable review but force fresh command preflight',async()=>{
  const h=setup();await h.ready();const review=h.session.review('start');assert.ok(review);
  h.frame('event: zone.state\ndata: {"room":"Den","volume":21}\n\n');await settle();
  assert.equal(h.session.state.pending,review);assert.equal(h.controller.canControl(),false);
  await h.t.tick(350);assert.equal(h.session.state.pending,review);assert.equal(h.controller.canControl(),true);
  assert.equal(await h.session.confirm(review),true);assert.equal(h.writes().length,1);h.close();
});

test('late JSON from before a disconnect cannot overwrite the recovered snapshot',async()=>{
  const h=setup();await h.ready();const stalled=deferred();let once=true;
  h.override((url)=>{if(url==='/zones'&&once){once=false;return stalled.promise;}});
  h.live.refresh();await h.t.tick(350);h.streams[0].source.error(new Error('lost connection'));await settle();
  h.data.title='Current';await h.t.tick(1350);assert.equal(h.controller.selected().state.track.title,'Current');
  stalled.resolve(new Response('[]'));await settle();assert.equal(h.controller.state.items.length,1);
  assert.equal(h.controller.selected().state.track.title,'Current');h.close();
});

test('the real page binding cancels command preflight reads without dispatching their command',async()=>{
  const h=setup();await h.ready();const stalled=deferred();h.override(url=>url.endsWith('/state')?stalled.promise:undefined);
  const action=h.controller.command('pause');await settle();h.streams[0].source.error(new Error('lost'));
  assert.equal(await action,false);assert.equal(h.writes().length,0);assert.equal(h.controller.state.uncertain,false);
  stalled.resolve(new Response(JSON.stringify(h.playback())));h.close();await settle();
});

test('the real page binding does not cancel, forget or replay a command sent before disconnection',async()=>{
  const h=setup();await h.ready();const stalled=deferred();h.override((url,init)=>init.method==='POST'?stalled.promise:undefined);
  const action=h.controller.command('pause');await settle();assert.equal(h.writes().length,1);
  h.streams[0].source.error(new Error('lost'));await h.t.tick(2000);
  assert.equal(h.writes()[0].init.signal.aborted,false);assert.equal(h.controller.state.busy,true);
  stalled.resolve(new Response('{"done":"Accepted once","changed":true}'));assert.equal(await action,true);
  await h.t.tick(2000);assert.equal(h.writes().length,1);assert.equal(h.controller.state.message,'Accepted once');h.close();
});

test('reset recovery retains uncertain outcomes until the actual explicit Refresh path succeeds',async()=>{
  const h=setup();await h.ready();h.override((url,init)=>{if(init.method==='POST')throw new Error('reply lost');});
  await h.controller.command('pause');h.frame('event: events.reset\ndata: {}\n\n');await h.t.tick(350);
  assert.equal(h.controller.state.uncertain,true);assert.equal(h.controller.canControl(),false);
  vm.runInContext('kick(true)',h.context);await h.t.tick(0);assert.equal(h.controller.state.uncertain,false);
  assert.equal(h.writes().length,1);h.close();
});

test('missing event capability still exposes fresh snapshots rather than claiming live continuity',async()=>{
  const h=setup();h.context.RemoteEventFeed=undefined;
  await h.live.resume();await h.t.tick(350);assert.equal(h.controller.state.connected,false);
  assert.equal(h.controller.canControl(),true);await h.t.tick(15000);assert.equal(h.snapshotCount(),2);h.close();
});

test('the served asset order installs heartbeat delivery and recovery before page startup',()=>{
  const rust=fs.readFileSync(path.join(__dirname,'../crates/fsonos-api/src/http/remote/dashboard.rs'),'utf8');
  assert.match(rust,/const EVENTS: &str = include_str!\("\.\.\/\.\.\/remote\/event-feed\.js"\);/);
  assert.match(rust,/const LIVE: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-live\.js"\);/);
  assert.ok(rust.includes('{EVENTS}\\n{CORE}\\n{LIVE}\\n{VISUALIZER}'));
  assert.match(script,/live\.suspend\(\); request\.cancelReads\(\);/);
  assert.match(script,/live\.resume\(\)/);
  assert.doesNotMatch(script,/schedule\(changedDuringRead \? 500 : wait\)/);
});

test('a partial snapshot failure keeps successfully observed rooms usable in fallback',async()=>{
  const h=setup();await h.ready();
  const other={household:'S1',coordinator_room:'Study',members:['Study'],transport_state:'paused'};
  h.override((url)=>url==='/zones'?new Response(JSON.stringify([h.playback().zone,other]))
    :url.includes('Study%40S1')?new Response('{"detail":"Offline"}',{status:503}):undefined);
  h.live.refresh();await h.t.tick(350);
  assert.equal(h.controller.state.connected,false);assert.equal(h.controller.state.items.length,2);
  assert.equal(h.controller.selected().fresh,true);assert.equal(h.controller.canControl(),true);
  assert.equal(h.controller.state.items[1].fresh,false);assert.match(h.controller.state.error,/Some rooms/);
  h.close();
});

test('ordinary clock advancement cannot expire DJ controls without an on-demand review read',()=>{
  const start=script.indexOf('  async function review(kind, args) {');
  assert.ok(start>=0,'the page review handler must be asynchronous');
  const review=script.slice(start,script.indexOf('\n  function renderReview',start));
  assert.match(review,/await session\.load\(\)/);
  assert.match(review,/target !== controller\.state\.selected/);
  assert.match(review,/finally \{ reviewReading = false; controls\(\); \}/);
});
