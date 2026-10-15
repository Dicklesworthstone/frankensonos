// Live queue snapshots use the shipped controller; no packages or speaker access.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const source = fs.readFileSync(process.env.QUEUE_SOURCE || path.join(__dirname, '../crates/fsonos-api/src/remote/queue.js'), 'utf8');
const Queue = vm.runInNewContext(source + '\nQueueRemote;');
const plain = v => JSON.parse(JSON.stringify(v));
const settle = async () => { for (let i = 0; i < 60; i++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return { promise, resolve, reject }; }
function page(offset = 0, total = 60, revision = 'q:1', playing = 2) {
  return { total, revision, playing, items: Array.from({length: Math.min(25, Math.max(0,total-offset))}, (_,i) =>
    ({position:offset+i+1,title:`Track ${offset+i+1}`,uri:`render:${offset+i+1}`})) };
}
function harness() {
  const calls = []; let reply = (route,args) => route === 'queue' ? page(args.query.offset) : {done:'Played',changed:true};
  let blocked = false, owned = true;
  const c = Queue.model({zone:'Room / ?@S1', blocked:()=>blocked, current:()=>owned,
    call(route,args) { calls.push([route,plain(args)]); return reply(route,args); }});
  return { c,calls, reply(fn) { reply=fn; }, block(v) { blocked=v; c.block(); }, own(v) { owned=v; },
    reads:()=>calls.filter(([r])=>r==='queue'), writes:()=>calls.filter(([r])=>r!=='queue') };
}

test('unopened and closed queues make no live reads and cannot reopen themselves', async () => {
  const h=harness(); h.c.stale(); await h.c.reconcile(); assert.equal(h.calls.length,0);
  await h.c.load(); h.c.close(); h.c.stale(); await h.c.reconcile(); assert.equal(h.calls.length,1);
  await h.c.load(25); assert.equal(h.c.state.page.items[0].position,26);
});

test('healthy open queues do not poll or reread when reconciliation has nothing dirty', async () => {
  const h=harness(); await h.c.load();
  for(let i=0;i<100;i++) await h.c.reconcile();
  assert.equal(h.reads().length,1); assert.equal(h.c.state.stale,false);
});

test('one event refresh updates total and current marker without a playback command', async () => {
  const h=harness(); await h.c.load(); h.reply((r,a)=>page(a.query.offset,65,'q:2',4));
  h.c.stale(); assert.equal(await h.c.play(1),false); assert.equal(h.c.state.stale,true);
  assert.equal(await h.c.reconcile(),true); assert.equal(h.c.state.page.playing,4); assert.equal(h.c.state.page.total,65);
  assert.deepEqual(h.calls.map(([r])=>r),['queue','queue']);
  assert.deepEqual(h.calls[1][1], {params:{room:'Room / ?@S1'},query:{offset:0,limit:25}});
});

test('same revision and exact entry identities preserve selection, source and destination drafts', async () => {
  const h=harness(); await h.c.load(); h.c.select(2,true); h.c.select(3,true);
  h.c.state.source='spotify:album:0123456789abcdefghijkl'; h.c.state.destination='59';
  h.reply((r,a)=>{const p=page(a.query.offset); p.playing=3; p.items[1].title='Improved metadata'; return p;});
  h.c.stale(); await h.c.reconcile();
  assert.deepEqual([...h.c.state.selected],[2,3]); assert.equal(h.c.state.destination,'59');
  assert.equal(h.c.state.source,'spotify:album:0123456789abcdefghijkl');
  assert.equal(h.c.state.page.items[1].title,'Improved metadata'); assert.equal(h.c.removable().length,2);
});

for(const change of [p=>p.revision='q:2',p=>p.items[1].uri='render:replacement',p=>p.total=61,p=>p.revision=null]) {
  test('a changed revision or inconsistent entry identity retires selected positions', async () => {
    const h=harness(); await h.c.load(); h.c.select(2,true); h.c.state.destination='59'; h.c.state.source='unsent draft';
    h.reply((r,a)=>{const p=page(a.query.offset);change(p);return p;});
    h.c.stale(); await h.c.reconcile();
    assert.equal(h.c.state.selected.size,0); assert.equal(h.c.state.destination,''); assert.equal(h.c.state.source,'unsent draft');
    assert.equal(h.writes().length,0);
  });
}

test('notifications consume removal and move reviews even when the next revision is unchanged', async () => {
  for(const kind of ['remove','move']) {
    const h=harness(); await h.c.load(); h.c.select(2,true);
    if(kind==='remove')h.c.reviewRemoval();else h.c.reviewMove(61);
    const old=h.c.state.pending; h.c.stale(); assert.equal(h.c.state.pending,null);
    assert.equal(await h.c[kind](old),false); await h.c.reconcile();
    assert.equal(await h.c[kind](old),false); assert.equal(h.c.state.selected.size,1); assert.equal(h.writes().length,0);
  }
});

test('rapid invalidations coalesce into one bounded queue read', async () => {
  const h=harness(); await h.c.load(); const wait=deferred(); h.reply(()=>wait.promise);
  for(let i=0;i<100;i++)h.c.stale();
  const a=h.c.reconcile(); const b=h.c.reconcile(); assert.equal(a,b); await settle();
  assert.equal(h.reads().length,2); wait.resolve(page()); await a;
  assert.equal(h.c.state.stale,false); assert.equal(h.c.state.updating,false);
});

test('events received during a live read fence it and a later shared batch repairs it', async () => {
  const h=harness(); await h.c.load(); const wait=deferred(); h.reply(()=>wait.promise);
  h.c.stale(); const a=h.c.reconcile(); await settle();
  h.c.stale(); const b=h.c.reconcile(); assert.equal(a,b);
  wait.resolve(page(0,60,'obsolete')); await a; assert.notEqual(h.c.state.page.revision,'obsolete');
  assert.equal(h.c.state.stale,true); assert.equal(h.reads().length,2);
  h.reply(()=>page(0,60,'current')); await h.c.reconcile(); assert.equal(h.c.state.page.revision,'current');
});

test('automatic reconciliation waits for a manual read and never overlaps it', async () => {
  const h=harness(), wait=deferred(); let ongoing=0,peak=0;
  h.reply(async()=>{peak=Math.max(peak,++ongoing);try{return await wait.promise;}finally{ongoing--;}});
  const manual=h.c.load(); const automatic=h.c.reconcile(); await settle(); assert.equal(h.reads().length,1);
  wait.resolve(page()); await manual; await automatic; assert.equal(peak,1); assert.equal(h.reads().length,1);
});

test('an event during a manual read schedules one fresh read after the obsolete sample settles', async () => {
  const h=harness(), wait=deferred(); h.reply(()=>h.reads().length===1?wait.promise:page(0,60,'new'));
  const manual=h.c.load(); h.c.stale(); const automatic=h.c.reconcile(); await settle();
  assert.equal(h.reads().length,1); wait.resolve(page(0,60,'old')); await manual; await automatic;
  assert.equal(h.reads().length,2); assert.equal(h.c.state.page.revision,'new');
});

test('a newer explicit page wins over delayed live results without losing its offset', async () => {
  const h=harness(); await h.c.load(); const wait=deferred();
  h.reply((r,a)=>a.query.offset===0?wait.promise:page(a.query.offset,60,'new'));
  h.c.stale(); const old=h.c.reconcile(); await settle(); await h.c.load(25);
  wait.resolve(page(0,60,'old')); await old;
  assert.equal(h.c.state.offset,25); assert.equal(h.c.state.page.revision,'new'); assert.equal(h.c.state.stale,false);
});

test('explicit pages retain latest-read-wins behavior', async () => {
  const h=harness(), a=deferred(), b=deferred(); h.reply(()=>h.calls.length===1?a.promise:b.promise);
  const first=h.c.load(0),second=h.c.load(25); b.resolve(page(25)); await second;
  a.resolve(page()); await first; assert.equal(h.c.state.offset,25); assert.equal(h.c.state.page.items[0].position,26);
});

test('background failures remain stale and propagate to the shared retry owner', async () => {
  const h=harness(); await h.c.load(); const before=h.c.state.page;
  const failure=Object.assign(new Error('offline'),{hint:'Reconnect the room.'}); h.reply(()=>Promise.reject(failure)); h.c.stale();
  await assert.rejects(h.c.reconcile(),e=>e===failure);
  assert.equal(h.c.state.stale,true); assert.equal(h.c.state.updating,false); assert.equal(h.c.state.page,before);
  assert.equal(await h.c.play(1),false); assert.match(h.c.state.message,/Reconnect/);
  h.reply(()=>page()); await h.c.reconcile(); assert.equal(h.c.state.stale,false); assert.equal(h.c.state.message,'');
});

test('malformed queue responses retain a disabled last-known snapshot and can recover', async () => {
  const h=harness(); await h.c.load(); h.reply(()=>({total:60,items:[]})); h.c.stale();
  await assert.rejects(h.c.reconcile(),/incomplete/); assert.equal(h.c.state.page.items.length,25);
  assert.equal(h.c.select(1,true),false); h.reply(()=>page()); await h.c.reconcile(); assert.equal(h.c.select(1,true),true);
});

test('shrinking or empty queues navigate off a vanished page with at most one correction', async () => {
  for(const total of [40,25,0]) {
    const h=harness(); await h.c.load(50); h.reply((r,a)=>page(a.query.offset,total,'shrunk')); h.c.stale();
    await h.c.reconcile(); assert.equal(h.reads().length,3);
    assert.equal(h.c.state.offset,total===40?25:0); assert.equal(h.c.state.page.total,total); assert.equal(h.writes().length,0);
  }
});

test('a queue that shrinks twice cannot trigger an unbounded corrective read loop', async () => {
  const h=harness(); await h.c.load(50);
  h.reply((r,a)=>page(a.query.offset,h.reads().length===2?40:10,'shrinking')); h.c.stale();
  await assert.rejects(h.c.reconcile(),/changed again/); assert.equal(h.reads().length,3); assert.equal(h.c.state.stale,true);
});

test('native actions suspend reconciliation until they release the shared action lane', async () => {
  const h=harness(); await h.c.load(); h.block(true); h.c.stale(); await h.c.reconcile(); assert.equal(h.reads().length,1);
  h.block(false); await h.c.reconcile(); assert.equal(h.reads().length,2); assert.equal(h.c.state.stale,false);
});

test('events cannot cause a read or overlapping mutation during an outstanding queue write', async () => {
  const h=harness(); await h.c.load(); const wait=deferred(); h.reply((r,a)=>r==='queue'?page(a.query.offset):wait.promise);
  const write=h.c.play(1); await settle(); h.c.stale(); await h.c.reconcile(); assert.equal(h.reads().length,1);
  assert.equal(await h.c.add('spotify:track:example'),false);
  wait.resolve({done:'Playing',changed:true}); await write; assert.equal(h.reads().length,2); assert.equal(h.writes().length,1);
});

test('lost or malformed write replies cannot be cleared by passive reads or notifications', async () => {
  for(const reply of [null,{},new Error('reply lost')]) {
    const h=harness(); await h.c.load(); h.reply(r=>r==='queue'?page():reply instanceof Error?Promise.reject(reply):reply);
    await h.c.play(1); assert.equal(h.c.state.uncertain,true);
    h.c.stale(); await h.c.reconcile(); h.block(true); h.block(false); await h.c.reconcile();
    assert.equal(h.reads().length,1); assert.match(h.c.state.message,/not repeated/); assert.equal(await h.c.play(1),false);
    h.c.suspend(); h.c.resume(); await h.c.reconcile(); assert.equal(h.c.state.uncertain,true);
    assert.equal(await h.c.load(),true); assert.equal(h.c.state.uncertain,false); assert.equal(h.writes().length,1);
  }
});

test('an unsuccessful explicit recovery preserves uncertainty and the unsent source draft', async () => {
  const h=harness(); await h.c.load(); h.c.state.source='unsent source'; h.reply(()=>Promise.reject(new Error('lost')));
  await h.c.add(h.c.state.source); assert.equal(h.c.state.uncertain,true);
  assert.equal(await h.c.load(),false); h.c.stale(); await h.c.reconcile();
  assert.equal(h.c.state.uncertain,true); assert.equal(h.c.state.source,'unsent source'); assert.equal(h.reads().length,2);
  assert.match(h.c.state.message,/unconfirmed/);
});

test('closing, hiding and disposal fence late live reads without starting extra work', async () => {
  for(const interrupt of ['close','suspend','dispose']) {
    const h=harness(); await h.c.load(); const wait=deferred(); h.reply(()=>wait.promise);
    h.c.stale(); const updating=h.c.reconcile(); await settle(); h.c[interrupt](); wait.resolve(page(0,60,'late')); await updating;
    assert.notEqual(h.c.state.page?.revision,'late'); await h.c.reconcile(); assert.equal(h.reads().length,2);
  }
});

test('hidden-page late failures do not fail the shared live batch', async () => {
  const h=harness(); await h.c.load(); const wait=deferred(); h.reply(()=>wait.promise);
  h.c.stale(); const updating=h.c.reconcile(); await settle(); h.c.suspend(); wait.reject(new Error('offline'));
  assert.equal(await updating,false); assert.equal(h.reads().length,2);
});

test('page resumption reads no queue until the shared fresh-state pass arrives', async () => {
  const h=harness(); await h.c.load(); h.c.suspend(); assert.equal(await h.c.load(),false);
  h.c.resume(); assert.equal(h.reads().length,1); assert.equal(h.c.state.stale,true);
  await h.c.reconcile(); assert.equal(h.reads().length,2); assert.equal(h.c.state.stale,false);
});

test('disposing a sent write stops readback but does not turn it into another command', async () => {
  const h=harness(); await h.c.load(); const wait=deferred(); h.reply(()=>wait.promise);
  const action=h.c.play(1); h.c.dispose(); wait.resolve({done:'Playing',changed:true}); assert.equal(await action,true);
  await h.c.reconcile(); assert.equal(h.reads().length,1); assert.equal(h.writes().length,1);
});

test('lost ownership prevents both explicit and automatic reads from detached cards', async () => {
  const h=harness(); await h.c.load(); h.own(false); h.c.stale(); await h.c.reconcile(); assert.equal(await h.c.load(),false);
  assert.equal(h.reads().length,1); assert.equal(await h.c.play(1),false);
});

test('invalid offsets cannot reopen or invalidate a usable snapshot', async () => {
  const h=harness(); await h.c.load(); const before=h.c.state.page;
  for(const offset of [-1,1.5,NaN,Infinity,4294967295]) assert.equal(await h.c.load(offset),false);
  assert.equal(h.c.state.page,before); assert.equal(h.c.state.stale,false); assert.equal(h.reads().length,1);
});

test('live legacy pages remain browsable but do not permit unrevisioned edits', async () => {
  const h=harness(); await h.c.load(); h.reply(()=>page(0,2,null)); h.c.stale(); await h.c.reconcile();
  assert.equal(h.c.state.page.items.length,2); assert.equal(h.c.state.stale,false);
  assert.equal(await h.c.play(1),false); assert.equal(h.c.select(1,true),false); assert.equal(h.writes().length,0);
});

test('an executor retaining its callback after returning cannot dispatch an abandoned command', async () => {
  let saved; const calls=[];
  const c=Queue.model({zone:'Den@S2',execute:work=>{saved=work;},call:async route=>{calls.push(route);return page();}});
  await c.load(); assert.equal(await c.play(1),false); await saved(); assert.deepEqual(calls,['queue']);
});

test('automatic refresh waits through a manual read correcting its vanished last page', async () => {
  const h=harness(), first=deferred(), corrected=deferred(); let ongoing=0,peak=0;
  h.reply(async()=>{peak=Math.max(peak,++ongoing);try{return await (h.reads().length===1?first:corrected).promise;}finally{ongoing--;}});
  const manual=h.c.load(50); const auto=h.c.reconcile(); first.resolve(page(50,40));await settle();
  assert.equal(h.reads().length,2);assert.equal(peak,1);
  corrected.resolve(page(25,40));await manual;await auto;assert.equal(h.reads().length,2);assert.equal(peak,1);
});
