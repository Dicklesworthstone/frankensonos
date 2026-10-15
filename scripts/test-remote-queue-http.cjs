// Real loopback HTTP and the production request/queue controllers. No Sonos needed.
'use strict';
const { test }=require('node:test'), assert=require('node:assert/strict');
const fs=require('node:fs'), path=require('node:path'), vm=require('node:vm');
const http=require('node:http'), { once }=require('node:events');
const root=path.join(__dirname,'../crates/fsonos-api/src/remote');
const source=fs.readFileSync(path.join(root,'queue.js'),'utf8')+'\n'
  +fs.readFileSync(path.join(root,'remote.js'),'utf8').replace(/\nstart\(\);\s*$/,'');
const target='Room & ?/Æ@S1';
const page=(offset,total=60,revision='q:1',playing=1)=>({total,revision,playing,
  items:Array.from({length:Math.min(25,Math.max(0,total-offset))},(_,i)=>({position:offset+i+1,title:`Track ${offset+i+1}`,uri:`render:${offset+i+1}`}))});
async function fixture(t, handle) {
  const seen=[];
  const server=http.createServer(async(req,res)=>{
    try {
      const parts=[];for await(const chunk of req)parts.push(chunk);
      const body=parts.length?JSON.parse(Buffer.concat(parts).toString('utf8')):null;
      const entry={method:req.method,url:req.url,body,contentType:req.headers['content-type']};seen.push(entry);
      await handle(entry,req,res);
    } catch(error) {res.statusCode=500;res.end(JSON.stringify({detail:error.message}));}
  });
  server.listen(0,'127.0.0.1');await once(server,'listening');
  t.after(()=>{server.closeAllConnections();server.close();});
  const origin=`http://127.0.0.1:${server.address().port}`;
  const api=vm.runInNewContext(source+'\n({QueueRemote,call});',{
    AbortController,URLSearchParams,setTimeout,clearTimeout,fetch:(where,init)=>fetch(origin+where,init),
  });
  const c=api.QueueRemote.model({zone:target,call:api.call});
  t.after(()=>c.dispose());
  return {c,seen};
}
function json(res,value,status=200) {res.statusCode=status;res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));}
const offset=entry=>Number(new URL(entry.url,'http://fixture.invalid').searchParams.get('offset'));

test('live queue HTTP refreshes use qualified bounded pages and preserve same-revision selections',async t=>{
  let revision='q:1',playing=26;
  const h=await fixture(t,(entry,req,res)=>json(res,page(offset(entry),60,revision,playing)));
  assert.equal(await h.c.load(25),true);h.c.select(26,true);playing=27;
  h.c.stale();assert.equal(await h.c.reconcile(),true);assert.equal(h.c.state.page.playing,27);
  assert.deepEqual([...h.c.state.selected],[26]);revision='q:2';h.c.stale();await h.c.reconcile();
  assert.equal(h.c.state.selected.size,0);assert.equal(h.seen.length,3);
  for(const entry of h.seen){assert.equal(entry.method,'GET');assert.equal(entry.body,null);
    assert.equal(entry.url,'/zones/Room%20%26%20%3F%2F%C3%86%40S1/queue?offset=25&limit=25');}
});

test('a dropped real queue-play reply is never replayed or cleared by passive reconciliation',async t=>{
  let playing=1;
  const h=await fixture(t,(entry,req,res)=>{
    if(entry.method==='POST'){playing=entry.body.position;req.socket.destroy();return;}
    json(res,page(offset(entry),60,'q:1',playing));
  });
  await h.c.load();assert.equal(await h.c.play(2),false);assert.equal(h.c.state.uncertain,true);
  for(let n=0;n<20;n++){h.c.stale();await h.c.reconcile();}
  assert.equal(h.seen.length,2);assert.equal(await h.c.play(2),false);
  assert.equal(await h.c.load(),true);assert.equal(h.c.state.page.playing,2);assert.equal(h.c.state.uncertain,false);
  const writes=h.seen.filter(e=>e.method==='POST');assert.equal(writes.length,1);
  assert.equal(writes[0].contentType,'application/json');assert.equal(writes[0].url,'/queue/play');
  assert.deepEqual(writes[0].body,{zone:target,position:2,expected_uri:'render:2',expected_revision:'q:1'});
});

test('a real failing queue read reaches the shared-recovery caller and a later read repairs it',async t=>{
  let fail=false;
  const h=await fixture(t,(entry,req,res)=>fail?json(res,{code:'NOT_READY',detail:'queue unavailable',hint:'Read again later.'},500):json(res,page(offset(entry))));
  await h.c.load();fail=true;h.c.stale();
  await assert.rejects(h.c.reconcile(),e=>e.status===500&&e.code==='NOT_READY');
  assert.equal(h.c.state.stale,true);assert.equal(h.c.select(1,true),false);assert.match(h.c.state.message,/Read again later/);
  fail=false;await h.c.reconcile();assert.equal(h.c.state.stale,false);
  assert.equal(h.seen.length,3);assert.ok(h.seen.every(e=>e.method==='GET'));
});

test('a shrinking queue corrects the HTTP page once and never starts or edits playback',async t=>{
  let total=60;
  const h=await fixture(t,(entry,req,res)=>json(res,page(offset(entry),total,total===60?'q:1':'q:2')));
  await h.c.load(50);total=20;h.c.stale();await h.c.reconcile();
  assert.equal(h.c.state.offset,0);assert.equal(h.c.state.page.total,20);
  assert.deepEqual(h.seen.map(offset),[50,50,0]);assert.ok(h.seen.every(e=>e.method==='GET'));
});
