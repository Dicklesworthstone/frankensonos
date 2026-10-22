'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const S = require('../crates/fsonos-api/src/remote/dashboard-session.js');
const clone = (v) => structuredClone(v);
const zone = (h = 'S2') => ({ household: h, coordinator_room: 'Studio', members: ['Studio', 'Kitchen'], transport_state: 'playing' });
const track = () => ({ uri: 'synthetic:movement', title: 'II. Andante', queue_position: 2, position_secs: 70, duration_secs: 400 });
const work = () => ({ composer: 'Synthetic composer', title: 'Three rooms', movements: 3, movement: 2,
  performers: ['Synthetic quartet'], album: 'Study', minutes: 21, movement_title: 'II. Andante',
  reason: { summary: 'Artist spacing and preferences.', factors: [{ factor: 'artist_spacing', weight: 1400 }, { factor: 'feedback', weight: 800 }], relaxed: [] } });
const dj = () => ({ zone: 'Studio', running: true, now: work(), next: [{ ...work(), movement: undefined }],
  steering: { source: 'session', mood: 'focus', constraints: { energy_bias: -1 }, added: {}, expires_at: 1800000000, expires_in_secs: 3600 } });
const scene = () => ({ name: 'Dinner', saved: 1700000000, groups: [{ coordinator: 'Studio@S2', members: ['Kitchen@S2'], source: { kind: 'dj', mood: 'dinner' }, playing: true }],
  volumes: { 'Studio@S2': 22, 'Kitchen@S2': 18 }, mutes: { 'Kitchen@S2': false } });
const queueScene = () => ({ ...scene(), groups: [{ ...scene().groups[0], source: { kind: 'queue', queue: {
  coordinator: 'RINCON_SYNTHETIC', household: 'Sonos_fixture', update_id: 17, track: 3,
  position_secs: 70, play_mode: 'REPEAT_ALL', boot_seq: 9 } } }] });
const hit = () => ({ kind: 'track', title: 'A song', subtitle: 'An artist', source_uri: 'spotify:track:synthetic', score: 1 });
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a;reject=b; });return {resolve,reject,promise}; };
const tick = () => new Promise((r) => setImmediate(r));
function fixture({demo=false}={}) {
  let time = 1000, hook=null;
  const data = { zones:[zone(), zone('S1')], states: new Map(), dj:dj(), scene:scene(), moods:{moods:[{name:'focus',constraints:{energy_bias:-1}},{name:'dinner',constraints:{energy_bias:0}}]}, hits:[hit()] };
  for(const z of data.zones) data.states.set(C.key(z),{zone:z,transport_state:'playing',volume:20,group_volume:22,track:track()});
  const calls=[];
  const call=async(url,body)=>{
    calls.push({url,body:clone(body)});
    if(hook) { const v=hook(url,body); if(v!==undefined) return v; }
    if(url==='/zones') return clone(data.zones);
    if(url==='/dj/moods') return clone(data.moods);
    if(url==='/scenes') return [clone(data.scene)];
    if(url==='/scenes/Dinner') return clone(data.scene);
    if(url.endsWith('/state')) return clone(data.states.get(decodeURIComponent(url.split('/')[2])));
    if(url.endsWith('/dj')) return clone(data.dj);
    if(url.startsWith('/library/search')) return clone(data.hits);
    if(body!==undefined){
      if(url==='/dj/feedback') return {zone:'Studio',signal:body.signal,work:'Synthetic composer: Three rooms',composer:data.dj.now.composer,done:'Feedback recorded'};
      if(url==='/scenes/Dinner/apply') return {scene:'Dinner',done:'Scene applied',changed:true,complete:true,steps:['Group','Set volumes']};
      return {done:'Accepted',changed:true};
    }
    throw new Error(`Unexpected request ${url}`);
  };
  const controller=C.model({call,demo,now:()=>time});
  const session=S.model({call,controller,demo,now:()=>time});
  return {data,calls,call,controller,session,setHook(f){hook=f;},time(v){time=v;},writes:()=>calls.filter((c)=>c.body!==undefined),
    async ready(){await controller.refresh();await session.load();await session.catalog();return this;} };
}
test('strict decoding preserves whole movements and actual per-mille factors, without invented scores',()=>{
  const v=S.dj(dj(),zone());assert.equal(v.now.movements,3);assert.equal(v.now.reason.factors[0].weight,1400);
  assert.throws(()=>v.next.push(work()));assert.equal(S.djIdentity(v),S.djIdentity({...dj(),steering:{...dj().steering,expires_in_secs:5}}));
});
test('malformed current works, factor values, wrong rooms and invalid movement numbers are rejected',()=>{
  for(const value of [null,{...dj(),zone:'Elsewhere'},{...dj(),steering:null},{...dj(),now:{...work(),movement:4}},
    {...dj(),now:{...work(),reason:{summary:'x',factors:[{factor:'feedback',weight:1.5}]}}}])assert.throws(()=>S.dj(value,zone()));
});
test('scene and mood catalogs reject incomplete shapes and duplicated names',()=>{
  assert.equal(S.scenes([scene()])[0].volumes['Kitchen@S2'],18);
  for(const v of [null,[{...scene(),volumes:{bad:101}}],[scene(),scene()]])assert.throws(()=>S.scenes(v));
  assert.throws(()=>S.moods({moods:[{name:'focus',constraints:{}},{name:'focus',constraints:{}}]}));
});
test('dashboard queue scenes retain a frozen exact recall reference and explain its limit',()=>{
  const v = S.scene(queueScene());
  assert.deepEqual(v.groups[0].source.queue, queueScene().groups[0].source.queue);
  assert.equal(Object.isFrozen(v.groups[0].source.queue), true);
  assert.match(S.sceneSource(v.groups[0].source), /track #3 at 1:10/);
  assert.match(S.sceneSource(v.groups[0].source), /original player and unchanged queue/);
  for (const bad of [null, {}, { ...v.groups[0].source.queue, track: 0 },
    { ...v.groups[0].source.queue, play_mode: 'unknown' }]) {
    const value = queueScene(); value.groups[0].source.queue = bad;
    assert.throws(() => S.scene(value));
  }
  const hidden = queueScene(); hidden.groups[0].source.kind = 'keep';
  assert.throws(() => S.scene(hidden));
});
test('dashboard queue recall rechecks the entire saved reference before writing',async()=>{
  for (const changed of [false, true]) {
    const f = fixture(); f.data.scene = queueScene(); await f.ready();
    const reviewed = await f.session.reviewScene('Dinner');
    assert.equal(reviewed.scene.groups[0].source.queue.track, 3);
    if (changed) f.data.scene.groups[0].source.queue.boot_seq += 1;
    assert.equal(await f.session.confirm(reviewed), !changed);
    assert.equal(f.writes().length, changed ? 0 : 1);
    if (!changed) assert.deepEqual(f.writes()[0], { url: '/scenes/Dinner/apply', body: {} });
  }
});
test('session catalog and status are read-only, and movement count is not a track queue length',async()=>{
  const f=await fixture().ready();assert.equal(f.session.state.dj.now.movements,3);assert.equal(f.session.state.moods.length,2);assert.equal(f.writes().length,0);
});
test('steering review freezes exact scope, native mood and duration; chooses without writing',async()=>{
  const f=await fixture().ready();const r=f.session.review('steer',{mood:'dinner',for_secs:3600});
  assert.equal(r.label,'Studio + Kitchen');assert.deepEqual(r.body,{zone:'Studio@S2',mood:'dinner',for_secs:3600});
  assert.throws(()=>r.body.zone='Elsewhere');assert.equal(f.writes().length,0);assert.match(r.detail,/next pick/);
  assert.equal(await f.session.confirm(r),true);assert.deepEqual(f.writes(),[{url:'/dj/steer',body:r.body}]);
});
test('a catalog-defined mood is mandatory, durations are bounded and unrelated fields never enter a write',async()=>{
  const f=await fixture().ready();for(const args of [{mood:'fake'},{mood:'dinner',for_secs:0},{mood:'dinner',for_secs:NaN}])assert.equal(f.session.review('steer',args),null);
  const r=f.session.review('steer',{mood:'dinner',for_secs:1800,volume:100,source_uri:'evil'});
  assert.deepEqual(Object.keys(r.body),['zone','mood','for_secs']);
});
test('start uses the native start contract, while already-running sessions cannot start twice',async()=>{
  const f=await fixture().ready();assert.equal(f.session.review('start'),null);
  f.data.dj.running=false;await f.session.load();const r=f.session.review('start',{mood:'focus',for_secs:7200});
  assert.equal(await f.session.confirm(r),true);assert.deepEqual(f.writes()[0],{url:'/dj/start',body:{zone:'Studio@S2',mood:'focus',for_secs:7200}});
});
test('whole-work skip, stop, and program reset have distinct native contracts',async()=>{
  for(const [kind,url,body] of [['skip','/dj/skip',{zone:'Studio@S2'}],['stop','/dj/stop',{zone:'Studio@S2'}],['clear','/dj/steer',{zone:'Studio@S2',clear:true}]]){
    const f=await fixture().ready();const r=f.session.review(kind);assert.equal(await f.session.confirm(r),true);assert.deepEqual(f.writes()[0],{url,body});
  }
});
test('likes/dislikes record feedback about a checked work without a transport skip',async()=>{
  for(const kind of ['like','dislike']){
    const f=await fixture().ready();const r=f.session.review(kind);assert.equal(await f.session.confirm(r),true);
    assert.deepEqual(f.writes()[0],{url:'/dj/feedback',body:{zone:'Studio@S2',signal:kind}});
  }
});
test('a work or movement changing during review stops feedback and skip before dispatch',async()=>{
  for(const kind of ['like','skip']){
    const f=await fixture().ready();const r=f.session.review(kind);f.data.dj.now.movement=3;
    assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);assert.match(f.controller.state.message,/session or current work changed/);
  }
});
test('transport and duplicate-track queue slots are verified in addition to work metadata',async()=>{
  const f=await fixture().ready();const r=f.session.review('skip');f.data.states.get('Studio@S2').track.queue_position=3;
  assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
});
test('the passage of steering countdown seconds is not treated as a new session',async()=>{
  const f=await fixture().ready();const r=f.session.review('skip');f.data.dj.steering.expires_in_secs=3500;
  assert.equal(await f.session.confirm(r),true);
});
test('a changed mood definition is detected before it silently steers something different',async()=>{
  const f=await fixture().ready();const r=f.session.review('steer',{mood:'dinner'});f.data.moods.moods[1].constraints.energy_bias=2;
  assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);assert.match(f.controller.state.message,/mood definition changed/);
});
test('scene review reads the complete saved spec and whole-house topology without applying it',async()=>{
  const f=await fixture().ready();const r=await f.session.reviewScene('Dinner');assert.deepEqual(r.scene.volumes,scene().volumes);assert.equal(f.writes().length,0);
  assert.equal(await f.session.confirm(r),true);assert.deepEqual(f.writes()[0],{url:'/scenes/Dinner/apply',body:{}});
});
test('scene edits and changed grouping invalidate the review before the first scene command',async()=>{
  for(const change of ['scene','groups']){
    const f=await fixture().ready();const r=await f.session.reviewScene('Dinner');
    if(change==='scene')f.data.scene.volumes['Kitchen@S2']=50;else f.data.zones[0].members=['Studio'];
    assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
  }
});
test('partial scene results preserve failed and skipped steps without claiming a complete restoration',async()=>{
  const f=await fixture().ready();const r=await f.session.reviewScene('Dinner');
  f.setHook((url)=>url==='/scenes/Dinner/apply'?{scene:'Dinner',done:'Partly applied',changed:true,complete:false,steps:['Set volume'],failed:[{step:'Join',detail:'Offline'}],skipped:['Kitchen']}:undefined);
  assert.equal(await f.session.confirm(r),true);assert.equal(f.session.state.result.complete,false);assert.equal(f.session.state.result.failed.length,1);
  assert.match(f.controller.state.message,/Partial result/);assert.equal(f.writes().length,1);
});
test('search keeps unsupported sources read-only and plays a specific reviewed identity, not a rank',async()=>{
  const f=await fixture().ready();f.data.hits.push({title:'Unknown',kind:'future',source_uri:'javascript:bad'});await f.session.search('A song');
  assert.equal(f.session.review('play',{hit:f.session.state.hits[1]}),null);
  const r=f.session.review('play',{hit:f.session.state.hits[0]});assert.equal(await f.session.confirm(r),true);
  assert.deepEqual(f.writes()[0],{url:'/play',body:{zone:'Studio@S2',source_uri:'spotify:track:synthetic',title:'A song'}});
});
test('household favorites use the favorite endpoint, not a fabricated Spotify URI',async()=>{
  const f=await fixture().ready();f.data.hits=[{title:'Radio',kind:'favorite',favorite:'FV:2/7'}];await f.session.search('Radio');
  const r=f.session.review('play',{hit:f.session.state.hits[0]});assert.equal(await f.session.confirm(r),true);
  assert.deepEqual(f.writes()[0],{url:'/play/favorite',body:{zone:'Studio@S2',favorite:'FV:2/7'}});
});
test('a search result that no longer exists is not replaced with the new top result',async()=>{
  const f=await fixture().ready();await f.session.search('A song');const r=f.session.review('play',{hit:f.session.state.hits[0]});f.data.hits=[{...hit(),source_uri:'spotify:track:another'}];
  assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
});
test('the latest search wins over an old query response',async()=>{
  const f=await fixture().ready(),d=deferred();f.setHook(url=>url.includes('q=old')?d.promise:undefined);
  const first=f.session.search('old');await f.session.search('new');d.resolve([{...hit(),title:'Old'}]);await first;
  assert.equal(f.session.state.query,'new');assert.equal(f.session.state.hits[0].title,'A song');
});
test('switching targets refuses old reviews, including identical room names in another household',async()=>{
  const f=await fixture().ready();const r=f.session.review('skip');f.controller.select('Studio@S1');await f.session.load();
  assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);const next=f.session.review('skip');await f.session.confirm(next);
  assert.equal(f.writes()[0].body.zone,'Studio@S1');
});
test('native transport and session controls share one mutation lane and cannot interleave',async()=>{
  const f=await fixture().ready(),d=deferred();const r=f.session.review('skip');f.setHook((url,body)=>body?d.promise:undefined);
  const first=f.session.confirm(r);await tick();assert.equal(f.controller.state.busy,true);assert.equal(await f.controller.command('next'),false);
  assert.equal(await f.session.confirm(r),false);assert.equal(f.controller.select('Studio@S1'),false);assert.equal(f.writes().length,1);
  d.resolve({done:'Skipped',changed:true});await first;
});
test('cancel during preflight and hide during preflight prevent dispatch',async()=>{
  for(const cancel of ['cancel','suspend']){
    const f=await fixture().ready(),d=deferred();const r=f.session.review('skip');f.setHook(url=>url.endsWith('/dj')?d.promise:undefined);
    const first=f.session.confirm(r);await tick();f.session[cancel]();d.resolve(dj());assert.equal(await first,false);assert.equal(f.writes().length,0);
  }
});
test('late replies after a hidden sent command retain uncertainty and do not enable replay',async()=>{
  const f=await fixture().ready(),d=deferred();const r=f.session.review('skip');f.setHook((url,body)=>body?d.promise:undefined);
  const first=f.session.confirm(r);await tick();f.controller.suspend();f.session.suspend();d.reject(new Error('Lost'));
  await first;assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);f.controller.resume();f.session.resume();f.setHook(null);
  await f.controller.refresh();assert.equal(f.controller.canControl(),false);await f.controller.refresh(true);assert.equal(f.controller.canControl(),true);
});
test('malformed feedback acknowledgement is not a successful learning event',async()=>{
  const f=await fixture().ready();const r=f.session.review('like');f.setHook((url)=>url==='/dj/feedback'?{done:'ok',signal:'dislike',zone:'Studio'}:undefined);
  assert.equal(await f.session.confirm(r),false);assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
});
test('preview can explore real UI reviews but can never submit',async()=>{
  const f=await fixture({demo:true}).ready();for(const r of [f.session.review('skip'),await f.session.reviewScene('Dinner')]){
    assert.ok(r);assert.equal(await f.session.confirm(r),false);
  }assert.equal(f.writes().length,0);
});
test('expired DJ observations cannot generate fresh action reviews',async()=>{
  const f=await fixture().ready();f.time(50000);assert.equal(f.session.review('skip'),null);assert.equal(f.writes().length,0);
});
test('stored mutation callbacks cannot dispatch after the operation returned',async()=>{
  const f=await fixture().ready();let send;
  await f.controller.mutate(async(lease)=>{send=lease.send;return{done:'No action',changed:false};});
  await assert.rejects(send('/next',{zone:'Studio@S2'}),/view changed/);assert.equal(f.writes().length,0);
});
test('failed refresh preserves the previous timestamp instead of making an old position look recent',async()=>{
  const f=await fixture().ready();f.time(20000);f.setHook(url=>{if(url.endsWith('/state'))throw new Error('offline');});
  await f.controller.refresh();assert.equal(f.controller.selected().at,1000);assert.equal(f.controller.selected().fresh,false);
});
test('real loopback HTTP: reviewed scene application sends the exact native body once',async(t)=>{
  const f=await fixture().ready();const server=http.createServer(async(req,res)=>{let b='';for await(const chunk of req)b+=chunk;
    try{const value=await f.call(req.url,b?JSON.parse(b):undefined);res.setHeader('content-type','application/json');res.end(JSON.stringify(value));}
    catch(e){res.statusCode=500;res.end(JSON.stringify({detail:e.message}));}});
  await new Promise(r=>server.listen(0,'127.0.0.1',r));t.after(()=>new Promise(r=>{server.closeAllConnections();server.close(r);}));
  const base=`http://127.0.0.1:${server.address().port}`,call=C.request({fetcher:(u,i)=>fetch(base+u,i)});
  const controller=C.model({call}),session=S.model({call,controller});await controller.refresh();await session.catalog();
  const r=await session.reviewScene('Dinner');assert.equal(await session.confirm(r),true);assert.equal(f.writes().length,1);
  assert.deepEqual(f.writes()[0].body,{});
});
test('a feedback reply for another work by the same composer stays uncertain',async()=>{
  const f=await fixture().ready();const r=f.session.review('like');
  f.setHook(url=>url==='/dj/feedback'?{zone:'Studio',composer:'Synthetic composer',work:'Synthetic composer: Another work',signal:'like',done:'Recorded'}:undefined);
  assert.equal(await f.session.confirm(r),false);assert.equal(f.controller.state.uncertain,true);
});
test('incomplete scene sources and excessive room-level maps cannot be previewed as valid',()=>{
  for(const source of [{kind:'favorite'},{kind:'uri'},{kind:'dj',mood:17}])assert.throws(()=>S.scene({...scene(),groups:[{...scene().groups[0],source}]}));
  assert.throws(()=>S.scene({...scene(),volumes:Object.fromEntries(Array.from({length:257},(_,i)=>[String(i),5]))}));
});


test('a stale DJ movement cannot authorize feedback on a different observed source',async()=>{
  const f=await fixture().ready();f.data.states.get('Studio@S2').track.title='Different source';
  await f.controller.refresh();
  for(const kind of ['like','dislike','skip'])assert.equal(f.session.review(kind),null);
  assert.equal(f.writes().length,0);
});

test('newly mismatched movement metadata is checked again before feedback dispatch',async()=>{
  const f=await fixture().ready();const r=f.session.review('like');
  f.data.states.get('Studio@S2').track.title='Different source';
  assert.equal(await f.session.confirm(r),false);assert.equal(f.writes().length,0);
  assert.match(f.controller.state.message,/does not match playback/);
});
