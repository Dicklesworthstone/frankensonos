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
const Q = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const script = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/dashboard.js'), 'utf8');
const begin = script.indexOf('  live = DashboardLive.create({'), end = script.indexOf('  function suspend() {', begin);
assert.ok(begin >= 0 && end > begin, 'exercise the actual page wiring, not a duplicate');
const wiring = script.slice(begin, end);
const workspaceScript = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/dashboard-workspace.js'), 'utf8');
const invalidationStart = workspaceScript.indexOf('      invalidate(targets = null) {');
const invalidationEnd = workspaceScript.indexOf('      immerse(', invalidationStart);
assert.ok(invalidationStart >= 0 && invalidationEnd > invalidationStart, 'exercise the actual workspace invalidation');
const workspaceInvalidation = workspaceScript.slice(invalidationStart, invalidationEnd);
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
function setup(groups = null) {
  const t=clock(), requests=[], streams=[], data={title:'First song',state:'playing'}, gate={until:0};
  const zone={household:'S2',coordinator_room:'Den',members:['Den'],transport_state:'playing'};
  const zones=groups || [zone];
  let override=null, invalidations=0, histories=0;
  const playback=(group=zones[0])=>({zone:{...group},transport_state:data.state,volume:20,mute:false,
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
    const target=decodeURIComponent(url.split('/')[2] || '');
    const group=zones.find(z=>C.key(z)===target);
    const value=init.method==='POST'?{done:'Accepted',changed:true}:url==='/zones'?zones:url.endsWith('/state')?playback(group)
      :url.endsWith('/dj')?{zone:group?.coordinator_room,running:false,now:null,next:[],steering:{source:'none'}}
      :url.includes('/queue?')?{zone:group?.coordinator_room,total:3,revision:`queue:${target}`,playing:1,
        items:Array.from({length:3},(_,i)=>({position:i+1,title:`Track ${i+1}`,uri:`fixture:track:${i+1}`}))}
      :url==='/dj/moods'?{moods:[{name:'focus',constraints:{}}]}:[];
    return new Response(JSON.stringify(value),{headers:{'content-type':'application/json'}});
  };
  const request=C.request({fetcher,schedule:t.schedule,cancel:t.cancel});
  let queue;
  const controller=C.model({call:request,now:t.now,changed:()=>queue?.observe(controller.selected())});
  const session=S.model({call:request,controller,now:t.now});
  queue=Q.model({call:request});
  const workspace=vm.runInNewContext(`({${workspaceInvalidation}})`,{queue,controller,activity:null});
  const context=vm.createContext({controller,session,request,active:true,catalogsDirty:true,historyVersion:0,demo:false,
    workspace:{invalidate(targets){invalidations++;workspace.invalidate(targets);}},history:async()=>{histories++;},
    RemoteEventFeed:{open:(url)=>F.open(url,{fetcher,schedule:t.schedule,cancel:t.cancel,now:t.now,random:()=>0,gate})},
    DashboardLive:{create:options=>L.create({...options,schedule:t.schedule,cancel:t.cancel,now:t.now})},
  });
  vm.runInContext(wiring,context);
  const live=context.live;
  return {t,requests,streams,data,zones,playback,controller,session,queue,live,request,context,
    override(fn){override=fn;},get histories(){return histories;},get invalidations(){return invalidations;},
    frame(text,index=streams.length-1){streams[index].source.enqueue(new TextEncoder().encode(text));},
    async ready(){live.resume();await t.tick(350);assert.equal(controller.state.connected,true);},
    snapshotCount:()=>requests.filter(r=>r.url==='/zones').length,
    writes:()=>requests.filter(r=>r.init.method==='POST'),
    close(){queue.dispose();live.dispose();request.cancelReads();},
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
  assert.ok(rust.includes('{EVENTS}\\n{CORE}\\n{SEEK}\\n{LIVE}\\n{VISUALIZER}'));
  assert.match(rust,/const PLAY_MODE: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-play-mode\.js"\);/);
  assert.match(rust,/const GROUPS: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-groups\.js"\);/);
  assert.match(rust,/const TIMERS: &str = include_str!\("\.\.\/\.\.\/remote\/timers\.js"\);/);
  assert.match(rust,/const SLEEP: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-sleep\.js"\);/);
  assert.match(rust,/const VOLUME: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-volume\.js"\);/);
  assert.match(rust,/const SCENES: &str = include_str!\("\.\.\/\.\.\/remote\/dashboard-scenes\.js"\);/);
  assert.ok(rust.includes('{WORKSPACE}\\n{PLAY_MODE}\\n{GROUPS}\\n{TIMERS}\\n{SLEEP}\\n{VOLUME}\\n{SCENES}\\n{SCHEDULES}\\n{ROUTINES}\\n{ANNOUNCEMENTS}\\n{ANNOUNCEMENTS_PANEL}\\n{STEERING}\\n{DIRECTION}\\n{DJ_PREFERENCES}\\n{PREFERENCES}\\n{DJ_SESSION}\\n{REHEARSAL}\\n{SOURCE_PLAYER}\\n{SOURCE}\\n{SCRIPT}'));
  assert.match(script,/live\.suspend\(\); request\.cancelReads\(\);/);
  assert.match(script,/live\.resume\(\)/);
  assert.doesNotMatch(script,/schedule\(changedDuringRead \? 500 : wait\)/);
});

test('a partial snapshot failure preserves live continuity for successfully observed rooms',async()=>{
  const h=setup();await h.ready();
  const other={household:'S1',coordinator_room:'Study',members:['Study'],transport_state:'paused'};
  h.override((url)=>url==='/zones'?new Response(JSON.stringify([h.playback().zone,other]))
    :url.includes('Study%40S1')?new Response('{"detail":"Offline"}',{status:503}):undefined);
  h.live.refresh();await h.t.tick(350);
  assert.equal(h.controller.state.connected,true);assert.equal(h.controller.state.items.length,2);
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

const room=(name,household='S2',members=[name])=>({household,coordinator_room:name,members,transport_state:'playing'});
const stateReads=h=>h.requests.filter(r=>r.url.endsWith('/state')).map(r=>decodeURIComponent(r.url.split('/')[2]));
const delta=(h,name,data)=>h.frame(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);

test('actual page routing refreshes only the event group without retiring another group or its queue review',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();h.queue.open();await settle();
  const selected=h.controller.selected(),choice=h.queue.select(2),review=h.session.reviewQueue(choice,'play');
  assert.ok(review);const before=h.requests.length;
  delta(h,'zone.state',{room:'Beta',volume:21});await settle();
  assert.equal(h.controller.selected(),selected);assert.equal(selected.fresh,true);
  assert.equal(h.controller.state.connected,true);assert.equal(h.controller.canControl(),true);
  assert.equal(Q.choiceOf(choice),choice);assert.equal(h.session.state.pending,review);
  await h.t.tick(350);
  assert.deepEqual(h.requests.slice(before).map(r=>r.url),['/zones/Beta%40S2/state']);
  assert.equal(h.controller.selected(),selected);assert.equal(Q.choiceOf(choice),choice);
  assert.equal(await h.session.confirm(review),true);
  assert.equal(h.writes().length,1);assert.equal(h.writes()[0].url,'/queue/play');h.close();
});

test('room, health and DJ events coalesce every matching household-qualified group',async()=>{
  const h=setup([room('Alpha','S1',['Alpha','Shared']),room('Beta','S2',['Beta','Shared']),room('Other')]);
  await h.ready();h.controller.select('Other@S2');const selected=h.controller.selected(),before=stateReads(h).length;
  delta(h,'zone.state',{room:'Shared',volume:25});
  delta(h,'player.health',{room:'Shared',health:'degraded'});
  delta(h,'dj.status',{zone:'Beta',running:true});await settle();
  assert.equal(selected.fresh,true);await h.t.tick(350);
  assert.deepEqual(stateReads(h).slice(before).sort(),['Alpha@S1','Beta@S2']);
  assert.equal(h.snapshotCount(),1);assert.equal(h.controller.selected(),selected);
  assert.equal(h.writes().length,0);h.close();
});

test('an unrelated group event cannot cancel a selected-group command already in preflight',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();const stalled=deferred();let once=true;
  h.override(url=>{if(url==='/zones/Alpha%40S2/state'&&once){once=false;return stalled.promise;}});
  const action=h.controller.command('pause');await settle();
  delta(h,'zone.state',{room:'Beta',volume:21});await settle();
  assert.equal(h.controller.selected().fresh,true);
  stalled.resolve(new Response(JSON.stringify(h.playback())));
  assert.equal(await action,true);assert.equal(h.writes().length,1);
  assert.deepEqual(JSON.parse(h.writes()[0].init.body),{zone:'Alpha@S2'});
  assert.equal(h.controller.state.message,'Accepted');h.close();
});

for(const stage of ['connect','body']) {
  test(`Alpha control preempts an already stalled Beta ${stage} read, then recovers Beta without another event`,async()=>{
    const h=setup([room('Alpha'),room('Beta')]);await h.ready();
    const old=deferred(),next=deferred(),posted=deferred();let reads=0;
    h.override((url,init)=>{
      if(init.method==='POST')return posted.promise;
      if(url==='/zones/Beta%40S2/state') {
        if(++reads===1)return stage==='connect'?old.promise:{ok:true,text:()=>old.promise};
        return next.promise;
      }
    });
    delta(h,'zone.state',{room:'Beta',volume:22});await h.t.tick(350);
    const obsolete=h.requests.at(-1);assert.equal(obsolete.url,'/zones/Beta%40S2/state');
    assert.equal(h.controller.state.loading,true);assert.equal(h.controller.canControl(),true);
    const action=h.controller.command('pause');await settle();
    assert.equal(obsolete.init.signal.aborted,true);assert.equal(h.controller.state.busy,true);
    assert.equal(h.controller.state.loading,false);assert.equal(h.controller.state.connected,true);
    assert.equal(h.writes().length,1);assert.equal(h.writes()[0].init.signal.aborted,false);
    await h.t.tick(1000);assert.equal(reads,1,'background reads wait for the command lane');
    h.data.state='paused';posted.resolve(new Response('{"done":"Paused Alpha","changed":true}'));
    assert.equal(await action,true);assert.equal(h.controller.selected().state.transport_state,'paused');
    assert.equal(h.controller.canControl(),true);
    await h.t.tick(1000);assert.equal(reads,2,'the interrupted target is requeued without a new event');
    assert.equal(h.controller.state.loading,true);assert.equal(h.controller.canControl(),true);
    const late={...h.playback(h.zones[1]),volume:99};
    old.resolve(stage==='connect'?new Response(JSON.stringify(late)):JSON.stringify(late));await settle();
    assert.equal(h.controller.state.loading,true,'an old body cannot release the new observation lane');
    assert.equal(h.controller.state.connected,true);assert.equal(h.controller.state.message,'Paused Alpha');
    assert.equal(h.controller.state.items[1].state.volume,20);assert.equal(h.controller.state.items[1].fresh,false);
    assert.equal(h.writes().length,1);
    next.resolve(new Response(JSON.stringify({...h.playback(h.zones[1]),volume:22})));await settle();
    assert.equal(h.controller.state.loading,false);assert.equal(h.controller.state.items[1].state.volume,22);
    assert.equal(h.controller.state.items[1].fresh,true);assert.equal(h.controller.state.uncertain,false);h.close();
  });
}

test('a superseded room read cannot acknowledge a lost command reply',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();const old=deferred();let reads=0;
  h.override((url,init)=>{
    if(init.method==='POST')throw new Error('Reply lost');
    if(url==='/zones/Beta%40S2/state'&&++reads===1)return old.promise;
  });
  delta(h,'zone.state',{room:'Beta',volume:22});await h.t.tick(350);
  assert.equal(await h.controller.command('next'),false);assert.equal(h.controller.state.uncertain,true);
  old.resolve(new Response(JSON.stringify({...h.playback(h.zones[1]),volume:99})));await settle();
  assert.equal(h.controller.state.uncertain,true);assert.equal(h.controller.state.items[1].state.volume,20);
  await h.t.tick(1000);assert.equal(h.controller.state.items[1].fresh,true);
  assert.equal(h.controller.state.uncertain,true);assert.equal(h.controller.canControl(),false);
  vm.runInContext('kick(true)',h.context);await h.t.tick(0);assert.equal(h.controller.state.uncertain,false);
  assert.equal(h.writes().length,1);h.close();
});

test('full reconciliation cannot be preempted as an unrelated-room read',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();const held=deferred();
  h.override(url=>url==='/zones/Beta%40S2/state'?held.promise:undefined);
  h.live.refresh(true);await h.t.tick(0);
  assert.equal(h.controller.state.loading,true);assert.equal(h.controller.canControl(),false);
  assert.equal(await h.controller.command('pause'),false);assert.equal(h.writes().length,0);
  assert.equal(h.requests.findLast(r=>r.url==='/zones/Beta%40S2/state').init.signal.aborted,false);
  held.resolve(new Response(JSON.stringify(h.playback(h.zones[1]))));await settle();
  assert.equal(h.controller.canControl(),true);h.close();
});

test('Alpha and Beta sharing one coordinator are invalidated and reread as their actual group',async()=>{
  const h=setup([room('Alpha','S2',['Alpha','Beta']),room('Other')]);await h.ready();const held=deferred();
  const before=stateReads(h).length;
  h.override(url=>url==='/zones/Alpha%40S2/state'?held.promise:undefined);
  delta(h,'zone.state',{room:'Beta',volume:22});await h.t.tick(350);
  assert.equal(h.controller.selected().fresh,false);assert.equal(h.controller.canControl(),false);
  assert.equal(await h.controller.command('pause'),false);assert.equal(h.writes().length,0);
  assert.deepEqual(stateReads(h).slice(before),['Alpha@S2']);
  held.resolve(new Response(JSON.stringify(h.playback())));await settle();
  assert.equal(h.controller.selected().fresh,true);assert.equal(h.controller.canControl(),true);h.close();
});

test('an event for a selected group member still fences its unsent command',async()=>{
  const h=setup([room('Alpha','S2',['Alpha','Member']),room('Beta')]);await h.ready();const stalled=deferred();
  h.override(url=>url==='/zones/Alpha%40S2/state'?stalled.promise:undefined);
  const action=h.controller.command('pause');await settle();delta(h,'player.health',{room:'Member',health:'offline'});await settle();
  stalled.resolve(new Response(JSON.stringify(h.playback())));
  assert.equal(await action,false);assert.equal(h.writes().length,0);
  assert.equal(h.controller.state.uncertain,false);assert.match(h.controller.state.message,/No command was sent/);h.close();
});

test('a shared member name retires the selected queue in each matching household',async()=>{
  const h=setup([room('Alpha','S1',['Alpha','Shared']),room('Beta','S2',['Beta','Shared'])]);await h.ready();
  h.controller.select('Beta@S2');h.queue.open();await settle();const choice=h.queue.select(2);
  assert.ok(choice);delta(h,'zone.state',{room:'Shared',queue_length:4});await settle();
  assert.equal(h.controller.selected().fresh,false);assert.throws(()=>Q.choiceOf(choice),/no longer current/);
  assert.equal(h.queue.state.selection,null);assert.equal(h.writes().length,0);h.close();
});

for(const [name,data] of [['zone.state',{}],['zone.state',{room:'Unknown'}],['zone.state',[]],
  ['player.health',{room:42}],['dj.status',{room:'Beta'}],['topology.changed',{rooms:2}],
  ['action.logged',{intent:'volume Beta',result:'Changed'}],['events.reset',{}]]) {
  test(`${name} with ${JSON.stringify(data)} retains full reconciliation`,async()=>{
    const h=setup([room('Alpha'),room('Beta')]);await h.ready();const before=stateReads(h).length;
    // A pending scoped read cannot narrow an unknown or topology-wide change.
    delta(h,'zone.state',{room:'Beta',volume:20});delta(h,name,data);await settle();
    assert.ok(h.controller.state.items.every(i=>!i.fresh));await h.t.tick(350);
    assert.equal(h.snapshotCount(),2);assert.deepEqual(stateReads(h).slice(before),['Alpha@S2','Beta@S2']);
    assert.equal(h.writes().length,0);h.close();
  });
}

test('malformed event JSON cannot preserve a guessed room scope',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();
  h.frame('event: zone.state\ndata: {broken\n\n');await settle();
  assert.ok(h.controller.state.items.every(i=>!i.fresh));await h.t.tick(350);
  assert.equal(h.snapshotCount(),2);assert.equal(h.writes().length,0);h.close();
});

test('healthy room updates bypass an offline room backoff without postponing its own recovery',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();let offline=true;
  h.override(url=>url==='/zones/Beta%40S2/state'&&offline?new Response('{"detail":"Offline"}',{status:503}):undefined);
  delta(h,'zone.state',{room:'Beta',volume:22});await h.t.tick(350);
  assert.deepEqual(h.controller.state.failedTargets,['Beta@S2']);assert.equal(h.controller.state.connected,true);
  assert.equal(h.controller.canControl(),true);const due=h.t.now()+30000;
  delta(h,'zone.state',{room:'Beta',volume:23});await h.t.tick(1000);
  delta(h,'zone.state',{room:'Alpha',volume:24});await h.t.tick(350);
  const healthy=h.controller.selected();assert.equal(healthy.fresh,true);
  assert.equal(stateReads(h).filter(r=>r==='Alpha@S2').length,2);
  assert.equal(stateReads(h).filter(r=>r==='Beta@S2').length,2);
  await h.t.tick(due-h.t.now()-1);
  assert.equal(stateReads(h).filter(r=>r==='Beta@S2').length,2);assert.equal(h.controller.canControl(),true);
  offline=false;await h.t.tick(1);
  assert.equal(h.snapshotCount(),2);assert.equal(stateReads(h).filter(r=>r==='Beta@S2').length,3);
  assert.equal(stateReads(h).filter(r=>r==='Alpha@S2').length,2);assert.equal(h.controller.selected(),healthy);
  assert.deepEqual(h.controller.state.failedTargets,[]);assert.equal(h.writes().length,0);h.close();
});

test('daemon Retry-After still gates healthy room reads while local room backoff is isolated',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();let offline=true;
  h.override(url=>url==='/zones/Beta%40S2/state'&&offline
    ?new Response('{"detail":"Wait"}',{status:503,headers:{'retry-after':'60'}}):undefined);
  delta(h,'player.health',{room:'Beta',health:'offline'});await h.t.tick(350);
  const before=h.requests.length,due=h.t.now()+60000;
  delta(h,'zone.state',{room:'Alpha',volume:25});await settle();
  for(let n=0;n<3;n++){await h.t.tick(15000);h.frame(': heartbeat\n\n');await settle();}
  await h.t.tick(due-h.t.now()-1);assert.equal(h.requests.length,before);
  offline=false;await h.t.tick(1);
  assert.equal(h.controller.selected().fresh,true);assert.deepEqual(h.controller.state.failedTargets,[]);
  assert.equal(h.writes().length,0);h.close();
});

test('scoped updates and failed-room recovery cannot acknowledge an uncertain write',async()=>{
  const h=setup([room('Alpha'),room('Beta')]);await h.ready();let offline=true;
  h.override((url,init)=>{
    if(init.method==='POST')throw new Error('Reply lost');
    if(url==='/zones/Beta%40S2/state'&&offline)return new Response('{"detail":"Offline"}',{status:503});
  });
  assert.equal(await h.controller.command('next'),false);assert.equal(h.controller.state.uncertain,true);
  delta(h,'zone.state',{room:'Beta',volume:25});await h.t.tick(350);
  assert.equal(h.controller.state.uncertain,true);assert.equal(h.controller.canControl(),false);
  offline=false;await h.t.tick(30000);
  assert.equal(h.controller.selected().fresh,true);assert.equal(h.controller.state.uncertain,true);
  assert.equal(h.controller.canControl(),false);
  vm.runInContext('kick(true)',h.context);await h.t.tick(0);
  assert.equal(h.controller.state.uncertain,false);assert.equal(h.controller.canControl(),true);
  assert.equal(h.writes().length,1);h.close();
});
