'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const A = require('../crates/fsonos-api/src/remote/dashboard-activity.js');
const clone = (v) => structuredClone(v);
const tick = () => new Promise((r) => setImmediate(r));
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a;reject=b; }); return {promise,resolve,reject}; };
const action = (changes = {}) => ({ id: 9, at: 1700000000, client: 'fixture-browser', surface: 'http',
  intent: 'set_volume: Synthetic room', decision: 'clamp: quiet hours', result: 'Volume is 20', undoable: true, ...changes });
const zone = (household = 'S2') => ({ household, coordinator_room: 'Synthetic room', members: ['Synthetic room'], transport_state: 'playing' });
const playback = (z = zone()) => ({ zone: z, transport_state: 'playing', volume: 20,
  track: { uri: 'fixture:song', title: 'Test song', duration_secs: 300, position_secs: 10 } });
function fixture({ demo = false, changed = () => {}, now } = {}) {
  const calls = [], data = { rows: [action()], candidate: action(), zones: [zone(),zone('S1')] };
  let hook = null;
  const call = async (url, body) => {
    calls.push({url,body:clone(body)});
    if (hook) { const answer = hook(url,body); if (answer !== undefined) return answer; }
    if (url === '/zones') return clone(data.zones);
    if (url.endsWith('/state')) return playback(data.zones.find(z => url.includes(encodeURIComponent(C.key(z)))));
    if (url.startsWith('/actions?')) return clone(data.rows);
    if (url === '/undo/preview') return clone(data.candidate);
    if (url === '/undo/checked') {
      assert.deepEqual(body,{expected_id:9});
      data.candidate = null;
      data.rows.unshift(action({id:10,undoable:false,undo_of:9,intent:'undo',result:'Restored'}));
      return {undone:9,intent:action().intent,summary:'Restored previous levels'};
    }
    if (body !== undefined) return {done:'Accepted',changed:true};
    throw new Error(`Unexpected request ${url}`);
  };
  const controller = C.model({call,demo,now:()=>1000});
  const model = A.model({call,controller,changed,now});
  return {calls,data,call,controller,model,hook(fn){hook=fn;},writes:()=>calls.filter(c=>c.body!==undefined),
    reads:()=>calls.filter(c=>c.url.startsWith('/actions?') || c.url==='/undo/preview'),
    async ready(){await controller.refresh();await model.refresh();return this;}};
}

test('action decoding retains exact identity, policy and store order without inventing eligibility', () => {
  const rows=A.actionsOf([action({id:10,at:10,undoable:false}),action({at:100})],20);
  assert.deepEqual(rows.map(r=>r.id),[10,9]); assert.equal(rows[0].undoable,false);
  assert.equal(rows[1].decision,'clamp: quiet hours'); assert.throws(()=>{rows[0].id=1;}); assert.throws(()=>rows.push(action()));
});
test('malformed IDs, dates and undo links never authorize an operation', () => {
  for(const row of [null,{},action({id:0}),action({id:2**53}),action({at:8640000000001}),action({undoable:1}),
    action({intent:null}),action({client:'a'.repeat(2049)}),action({undo_of:9}),action({undo_of:8}),action({undoable:false,undo_of:0})]) assert.throws(()=>A.actionOf(row));
  for(const rows of [null,{},[action(),action()],Array.from({length:21},(_,i)=>action({id:i+1}))]) assert.throws(()=>A.actionsOf(rows,20));
});
test('only an authoritative undoable preview can be reviewed; null is genuine absence', () => {
  assert.equal(A.previewOf(null),null);
  for(const v of [undefined,{},action({undoable:false}),action({client:' '}),action({intent:''}),action({undo_of:8})]) assert.throws(()=>A.previewOf(v));
});
test('receipt must identify the reviewed action and preserve all partial failures', () => {
  const response={undone:9,intent:action().intent,summary:'Partly restored',failures:['Synthetic room unavailable','Current cap retained']};
  const result=A.resultOf(response,action()); assert.equal(result.complete,false); assert.deepEqual(result.failures,response.failures);
  assert.throws(()=>result.failures.push('x'));
  for(const bad of [null,{}, {...response,undone:10},{...response,intent:'other'},{...response,summary:''},{...response,failures:null},{...response,failures:['']}]) assert.throws(()=>A.resultOf(bad,action()));
});
test('closed activity does no reads; opening reads history and independent preview only', async () => {
  const f=fixture();await f.controller.refresh();f.model.observe();assert.equal(f.reads().length,0);
  await f.model.refresh();assert.equal(f.reads().length,2);assert.equal(f.writes().length,0);assert.equal(f.model.state.stale,false);
  for(let i=0;i<50;i++)f.model.observe();await tick();assert.equal(f.reads().length,2);
});
test('history can be inspected without a selected speaker and never supplies the undo target', async () => {
  const f=fixture();f.data.zones=[];f.data.candidate=null;await f.ready();
  assert.equal(f.model.state.rows.length,1);assert.equal(f.model.state.previewLoaded,true);assert.equal(f.model.review(),null);
});
test('the authoritative preview can differ from the first history row', async () => {
  const f=fixture();f.data.rows=[action({id:80,client:'another-caller'})];await f.ready();
  const pending=f.model.review();assert.equal(pending.action.id,9);assert.equal(pending.action.client,'fixture-browser');
});
test('history and preview failures remain separate and do not erase a last-known log', async () => {
  const f=await fixture().ready();f.hook(u=>{if(u.startsWith('/actions?'))throw new Error('History unavailable');});
  await f.model.refresh();assert.equal(f.model.state.rows[0].id,9);assert.equal(f.model.state.stale,true);
  assert.equal(f.model.state.previewLoaded,true);assert.ok(f.model.review());
  f.hook(u=>{if(u==='/undo/preview')throw new Error('Policy denied');});await f.model.refresh();
  assert.equal(f.model.state.stale,false);assert.equal(f.model.review(),null);assert.match(f.model.state.previewError,/Policy denied/);
});
test('local history search never requests or reorders data, and cannot select an undo', async () => {
  const f=await fixture().ready();const count=f.reads().length;
  f.model.search('QUIET');assert.equal(f.model.visible().length,1);f.model.search('not present');assert.equal(f.model.visible().length,0);
  assert.equal(f.reads().length,count);assert.equal(f.model.state.candidate.id,9);
});
test('limit changes serialize behind the old read and keep only the newest intent', async () => {
  const f=fixture(),d=deferred();await f.controller.refresh();let n=0;
  f.hook(u=>u.startsWith('/actions?') && ++n===1 ? d.promise : undefined);
  const first=f.model.refresh();await tick();f.model.limit(100);f.model.limit(20);f.model.limit(100);
  assert.equal(f.reads().length,2);d.resolve([action({id:1})]);await first;await tick();
  assert.equal(f.reads().length,4);assert.equal(f.reads()[2].url,'/actions?limit=100');assert.equal(f.model.state.rows[0].id,9);
  assert.equal(f.model.limit(1000),false);
});
test('closing, suspending and disposal fence delayed observations', async () => {
  for(const method of ['close','suspend','dispose']){
    const f=fixture(),d=deferred();await f.controller.refresh();f.hook(u=>u.startsWith('/actions?')?d.promise:undefined);
    const first=f.model.refresh();await tick();f.model[method]();d.resolve([action()]);await first;await tick();
    assert.equal(f.model.state.loaded,false);assert.equal(f.model.state.previewLoaded,false);assert.equal(f.reads().length,2);
  }
});
test('a refresh queued during an old suspended read resumes without overlapping requests', async () => {
  const f=fixture(),d=deferred();await f.controller.refresh();let n=0;
  f.hook(u=>u.startsWith('/actions?') && ++n===1 ? d.promise:undefined);
  const first=f.model.refresh();await tick();f.model.suspend();f.model.resume();assert.equal(f.reads().length,2);
  d.resolve([action({id:1})]);await first;await tick();assert.equal(f.reads().length,4);assert.equal(f.model.state.rows[0].id,9);
});
test('an event burst queues one replacement read, and a failed read does not start polling', async () => {
  const f=await fixture().ready(),d=deferred();let n=0;
  f.hook(u=>u.startsWith('/actions?')&&++n===1?d.promise:undefined);
  const first=f.model.refresh();await tick();for(let i=0;i<100;i++){f.model.invalidate();f.model.observe();}
  assert.equal(f.reads().length,4);d.resolve([action()]);await first;await tick();assert.equal(f.reads().length,6);
  f.hook(()=>{throw new Error('Offline');});await f.model.refresh();const count=f.reads().length;
  for(let i=0;i<20;i++)f.model.observe();await tick();assert.equal(f.reads().length,count);
});
test('review is immutable, read-only and explicitly bound to the current view', async () => {
  const f=await fixture().ready(),r=f.model.review();assert.equal(f.writes().length,0);
  assert.equal(r.action.id,9);assert.throws(()=>{r.action.id=10;});assert.equal(f.model.review(),null);
  f.model.cancel();assert.equal(await f.model.confirm(r),false);assert.equal(f.writes().length,0);
});
test('confirm rereads the exact preview then sends only expected_id to the checked endpoint', async () => {
  const f=await fixture().ready(),r=f.model.review();assert.equal(await f.model.confirm(r),true);await tick();
  assert.deepEqual(f.writes(),[{url:'/undo/checked',body:{expected_id:9}}]);
  assert.equal(f.model.state.result.complete,true);assert.equal(f.model.state.candidate,null);
  assert.equal(f.model.state.rows[0].undo_of,9);assert.equal(await f.model.confirm(r),false);
});
for(const change of [null,action({id:10}),action({client:'other'}),action({result:'Different result'})]){
  test(`changed preview ${JSON.stringify(change?.id??null)} never substitutes another undo`, async () => {
    const f=await fixture().ready(),r=f.model.review();f.data.candidate=change;
    assert.equal(await f.model.confirm(r),false);assert.equal(f.writes().length,0);assert.match(f.model.state.message,/eligible action changed/);
  });
}
test('a target change or same-address membership change retires the old review', async () => {
  for(const type of ['target','membership']){
    const f=await fixture().ready(),r=f.model.review();
    if(type==='target')f.controller.select('Synthetic room@S1');
    else{f.data.zones[0].members.push('Synthetic member');await f.controller.refresh();}
    f.model.observe();assert.equal(await f.model.confirm(r),false);assert.equal(f.writes().length,0);
  }
});
test('invalidation, close, cancel and suspension during preflight prevent dispatch', async () => {
  for(const method of ['invalidate','close','cancel','suspend','dispose']){
    const f=await fixture().ready(),r=f.model.review(),d=deferred();f.hook(u=>u==='/undo/preview'?d.promise:undefined);
    const first=f.model.confirm(r);await tick();f.model[method]();d.resolve(action());
    assert.equal(await first,false);assert.equal(f.writes().length,0);
  }
});
test('connection loss cancels undo preflight through the existing core epoch', async () => {
  const f=await fixture().ready(),r=f.model.review(),d=deferred();f.controller.connection(true);
  f.hook(u=>u==='/undo/preview'?d.promise:undefined);const first=f.model.confirm(r);await tick();
  f.controller.connection(false);d.resolve(action());assert.equal(await first,false);assert.equal(f.writes().length,0);
});
test('double submission and transport commands share one write lane', async () => {
  const f=await fixture().ready(),r=f.model.review(),d=deferred();f.hook((u,b)=>b!==undefined?d.promise:undefined);
  const first=f.model.confirm(r);await tick();assert.equal(f.controller.state.busy,true);
  assert.equal(await f.model.confirm(r),false);assert.equal(await f.controller.command('next'),false);assert.equal(f.writes().length,1);
  d.resolve({undone:9,intent:action().intent,summary:'Restored'});assert.equal(await first,true);
});
test('partial restoration is acknowledged once with every failure visible, not advertised as full success', async () => {
  const f=await fixture().ready();f.hook((u,b)=>b?{undone:9,intent:action().intent,summary:'Partly restored',failures:['Fixture speaker unavailable']}:undefined);
  assert.equal(await f.model.confirm(f.model.review()),true);assert.equal(f.model.state.result.complete,false);
  assert.match(f.model.state.message,/Partial restoration/);assert.equal(f.writes().length,1);
});
test('a sent undo acknowledgement survives close/hide without repeat or invented rollback', async () => {
  const f=await fixture().ready(),d=deferred();f.hook((u,b)=>b?d.promise:undefined);
  const first=f.model.confirm(f.model.review());await tick();f.model.close();f.model.suspend();f.controller.suspend();
  d.resolve({undone:9,intent:action().intent,summary:'Restored'});assert.equal(await first,true);
  assert.equal(f.model.state.result.complete,true);assert.equal(f.writes().length,1);
});
for(const receipt of [null,{}, {undone:10,intent:action().intent,summary:'Wrong action'}]){
  test(`malformed receipt ${JSON.stringify(receipt)} remains uncertain through activity refresh`, async () => {
    const f=await fixture().ready();f.hook((u,b)=>b?receipt:undefined);
    assert.equal(await f.model.confirm(f.model.review()),false);assert.equal(f.controller.state.uncertain,true);
    f.hook(null);await f.model.refresh();await f.controller.refresh();assert.equal(f.model.review(),null);
    assert.equal(f.controller.state.uncertain,true);assert.equal(f.writes().length,1);
    await f.controller.refresh(true);await f.model.refresh();assert.ok(f.model.review());
  });
}
test('a refused checked undo never falls back to an unguarded endpoint or force', async () => {
  const f=await fixture().ready();f.hook((u,b)=>{if(b)throw Object.assign(new Error('Newer action exists'),{status:409,hint:'Review again'});});
  assert.equal(await f.model.confirm(f.model.review()),false);assert.equal(f.writes().length,1);
  assert.equal(f.writes()[0].url,'/undo/checked');assert.match(f.model.state.message,/Review again/);
});
test('preview mode and final disposal cannot issue undo writes', async () => {
  const f=await fixture({demo:true}).ready();assert.equal(f.model.review(),null);assert.equal(await f.model.confirm({}),false);
  f.model.dispose();f.model.resume();await f.model.refresh();assert.equal(f.writes().length,0);
});
test('view exceptions cannot turn a successful undo into replay', async () => {
  const f=await fixture({changed(){throw new Error('View unavailable');}}).ready();
  assert.equal(await f.model.confirm(f.model.review()),true);assert.equal(f.writes().length,1);
});
for(const outcome of ['ok','lost','changed']){
  test(`real loopback HTTP checked undo: ${outcome}`, {timeout:5000}, async(t)=>{
    const calls=[];let candidate=action(),writes=0;
    const server=http.createServer(async(req,res)=>{
      let raw='';for await(const chunk of req)raw+=chunk;
      calls.push({url:req.url,method:req.method,body:raw?JSON.parse(raw):null});
      res.setHeader('content-type','application/json');
      if(req.url==='/zones'){res.end(JSON.stringify([zone()]));return;}
      if(req.url.endsWith('/state')){res.end(JSON.stringify(playback()));return;}
      if(req.url.startsWith('/actions?')){res.end(JSON.stringify([action()]));return;}
      if(req.url==='/undo/preview'){res.end(JSON.stringify(candidate));return;}
      if(req.url==='/undo/checked'){
        writes++;
        if(outcome==='changed'){res.writeHead(409);res.end(JSON.stringify({detail:'Candidate changed'}));return;}
        candidate=null;
        if(outcome==='lost'){req.socket.destroy();return;}
        res.end(JSON.stringify({undone:9,intent:action().intent,summary:'Restored'}));return;
      }
      res.writeHead(404);res.end('{}');
    });
    await new Promise(r=>server.listen(0,'127.0.0.1',r));t.after(()=>new Promise(r=>{server.closeAllConnections();server.close(r);}));
    const base=`http://127.0.0.1:${server.address().port}`,call=C.request({fetcher:(u,i)=>fetch(base+u,i)});
    const controller=C.model({call}),model=A.model({call,controller});await controller.refresh();await model.refresh();
    const ok=await model.confirm(model.review());await tick();model.close();await tick();
    assert.equal(ok,outcome==='ok');assert.equal(writes,1);
    assert.deepEqual(calls.filter(c=>c.method==='POST'),[{url:'/undo/checked',method:'POST',body:{expected_id:9}}]);
    assert.equal(controller.state.uncertain,outcome!=='ok');
    model.dispose();
  });
}

for(const delay of [180000,Infinity]){
  test(`activity preserves server cooldown ${delay} through visibility and refresh intents`, async () => {
    let time=0;const f=await fixture({now:()=>time}).ready();
    f.hook(u=>{if(u==='/undo/preview')throw Object.assign(new Error('Rate limited'),{retryAfter:delay});});
    await f.model.refresh();const count=f.reads().length;
    f.model.suspend();f.model.resume();for(let i=0;i<20;i++){await f.model.refresh();f.model.observe();}
    assert.equal(f.reads().length,count);time=179999;await f.model.refresh();assert.equal(f.reads().length,count);
    time=180000;f.hook(null);await f.model.refresh();assert.equal(f.reads().length,count+(delay===Infinity?0:2));
  });
}
