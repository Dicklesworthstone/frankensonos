// Executes the shipped controller; no npm dependencies or speaker access.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const Remote = require('../crates/fsonos-api/src/remote/announcements.js');
const clone = (v) => JSON.parse(JSON.stringify(v));
const groups = () => [
  { household: 'S1', coordinator_room: 'Room', members: ['Room', 'Side'] },
  { household: 'S2', coordinator_room: 'Room', members: ['Room'] },
];
const good = () => ({ done: 'announced; everything put back', clean: true,
  households: [{ lead: 'Room', rooms: ['Room'], levels: [{ room: 'Room', volume: 20 }], outcome: 'finished' }] });
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture({ answer = good(), read = groups, completed, readFile } = {}) {
  const calls = [];
  const c = Remote.model({ completed, readFile, call: async (route, args) => {
    calls.push([route, args]);
    if (route === 'zones') return typeof read === 'function' ? read() : read;
    if (answer instanceof Error) throw answer;
    return answer;
  } });
  return { c, calls };
}
async function prepare(c, target = 'Room@S2') {
  await c.refresh();
  c.select(target, true);
  c.field('words', ' Dinner is ready ');
  assert.equal(c.review(), true);
}

test('initialization is lazy and no omitted-room broadcast can be submitted', async () => {
  const { c, calls } = fixture();
  assert.equal(calls.length, 0);
  assert.equal(c.review(), false);
  assert.equal(await c.confirm(), false);
  await c.refresh(); c.field('words', 'Hello');
  assert.equal(c.review(), false);
  assert.match(c.state.message, /Select at least one/);
  assert.deepEqual(calls.map(([route]) => route), ['zones']);
});

test('speech sends the exact reviewed text, voice, level and qualified rooms once', async () => {
  const { c, calls } = fixture();
  await prepare(c);
  c.field('voice', ' en '); c.field('volume', '0'); c.select('Side@S1', true);
  assert.equal(c.review(), true);
  const review = c.state.pending;
  assert.equal(Object.isFrozen(review.body.rooms), true);
  assert.equal(await c.confirm(review), true);
  assert.deepEqual(calls[2], ['announceAudio', { body: {
    rooms: ['Side@S1', 'Room@S2'], volume: 0, text: 'Dinner is ready', voice: 'en',
  } }]);
  assert.equal(calls.length, 3);
  assert.equal(c.state.report.clean, true);
  assert.equal(c.state.report.households[0].levels[0].volume, 20);
  assert.equal(await c.confirm(review), false);
  assert.equal(c.review(), false, 'a new announcement requires a fresh room read');
});

test('each generated chime excludes stale speech and voice fields', async () => {
  for (const chime of ['bell', 'beep', 'rise']) {
    const { c, calls } = fixture();
    await prepare(c); c.field('voice', 'voice'); c.field('source', 'chime'); c.field('chime', chime);
    assert.equal(c.review(), true); assert.equal(await c.confirm(), true);
    assert.deepEqual(calls[2][1].body, { rooms: ['Room@S2'], volume: 35, chime });
  }
});

test('same-name household rooms are distinct and neither is preselected', async () => {
  const { c } = fixture(); await c.refresh();
  assert.equal(c.state.selected.size, 0);
  assert.equal(c.select('Room', true), false);
  c.select('Room@S1', true); c.field('words', 'One'); c.review();
  assert.deepEqual(c.state.pending.body.rooms, ['Room@S1']);
});

test('speech length counts Unicode characters and invalid input never reaches the daemon', async () => {
  const { c, calls } = fixture(); await prepare(c);
  for (const words of ['', '  ', 'a'.repeat(1001), '\ud800']) {
    c.field('words', words); assert.equal(c.review(), false);
  }
  c.field('words', '🎵'.repeat(1000)); assert.equal(c.review(), true);
  assert.equal(c.state.pending.body.text.length, 2000);
  assert.equal(calls.length, 1);
});

test('invalid volume, voice and chime choices never become defaults', async () => {
  for (const [field, invalid] of [
    ['volume', ['', '-1', '101', '1.5', 'NaN', '1e2', ' 35']],
    ['voice', ['a'.repeat(257), 'vo\nice', '\ud800']],
    ['chime', ['', 'ding', '__proto__']],
  ]) {
    for (const value of invalid) {
      const { c, calls } = fixture(); await prepare(c);
      if (field === 'chime') c.field('source', 'chime');
      c.field(field, value); assert.equal(c.review(), false, `${field}: ${JSON.stringify(value)}`);
      assert.equal(calls.length, 1);
    }
  }
});

test('room reads reject partial and conflicting topology, but empty discovery is valid', async () => {
  for (const read of [null, {}, [{ household: 'S2', coordinator_room: 'Room' }],
    [{ ...groups()[0], members: ['Room', 'Room'] }], [groups()[0], groups()[0]],
    [{ ...groups()[0], members: ['Side'] }]]) {
    const { c } = fixture({ read }); assert.equal(await c.refresh(), false); assert.equal(c.review(), false);
  }
  const { c } = fixture({ read: [] }); assert.equal(await c.refresh(), true); assert.equal(c.review(), false);
});

test('edited, cancelled, refreshed and superseded reviews cannot be confirmed', async () => {
  for (const edit of [
    (c) => c.field('words', 'Different'), (c) => c.field('volume', '25'),
    (c) => c.field('source', 'chime'), (c) => c.select('Side@S1', true),
    (c) => c.cancel(), (c) => c.review(), (c) => c.refresh(),
  ]) {
    const { c, calls } = fixture(); await prepare(c); const old = c.state.pending;
    await edit(c); assert.equal(await c.confirm(old), false);
    assert.ok(calls.every(([route]) => route === 'zones'));
  }
});

test('selected group membership changes prevent dispatch after review', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups()
    : [{ ...groups()[1], members: ['Room', 'New member'] }] });
  await prepare(c); assert.equal(await c.confirm(), false);
  assert.match(c.state.message, /group changed/);
  assert.deepEqual(calls.map(([route]) => route), ['zones', 'zones']);
});

test('membership ordering and unrelated groups do not invalidate selected rooms', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups()
    : [groups()[1], { ...groups()[0], members: ['Side', 'Room'] },
      { household: 'S2', coordinator_room: 'Elsewhere', members: ['Elsewhere'] }] });
  await prepare(c, 'Side@S1'); assert.equal(await c.confirm(), true);
  assert.equal(calls.length, 3);
});

test('failed preflight reads preserve the hint and make no audio request', async () => {
  let reads = 0;
  const { c, calls } = fixture({ read: () => {
    if (++reads === 1) return groups();
    throw Object.assign(new Error('Unavailable'), { hint: 'Check the daemon.' });
  } });
  await prepare(c); assert.equal(await c.confirm(), false);
  assert.match(c.state.message, /Check the daemon/); assert.equal(calls.length, 2);
});

test('double confirmation, edits and refresh cannot overlap a pending preflight', async () => {
  const waiting = deferred(); let reads = 0;
  const { c, calls } = fixture({ read: () => ++reads === 1 ? groups() : waiting.promise });
  await prepare(c); const pending = c.state.pending; const first = c.confirm(pending);
  assert.equal(await c.confirm(pending), false); assert.equal(c.field('words', 'Next'), false);
  assert.equal(await c.refresh(), false); assert.equal(c.select('Side@S1', true), false);
  waiting.resolve(groups()); assert.equal(await first, true);
  assert.equal(calls.filter(([route]) => route === 'announceAudio').length, 1);
});

test('closing, disposing, and closing then reopening cancel unsent preflights', async () => {
  for (const action of ['close', 'dispose', 'reopen']) {
    const waiting = deferred(); let reads = 0;
    const { c, calls } = fixture({ read: () => ++reads === 1 ? groups() : waiting.promise });
    await prepare(c); const work = c.confirm();
    if (action === 'dispose') c.dispose(); else c.close();
    if (action === 'reopen') await c.refresh();
    waiting.resolve(groups()); assert.equal(await work, false);
    assert.equal(calls.length, 2); assert.equal(c.state.writing, false);
  }
});

test('closing after dispatch never cancels, repeats or forgets its result', async () => {
  const waiting = deferred(); const { c, calls } = fixture({ answer: waiting.promise });
  await prepare(c); const work = c.confirm();
  await new Promise(setImmediate); assert.equal(c.state.phase, 'announcing');
  c.close(); assert.equal(c.state.writing, true); assert.equal(await c.refresh(), false);
  waiting.resolve(good()); assert.equal(await work, true); assert.equal(c.state.report.clean, true);
  assert.equal(calls.length, 3);
});

test('lost, rejected or malformed write replies require explicit refresh and are not repeated', async () => {
  const invalid = [new Error('Disconnected'), null, {}, { ...good(), households: [] },
    { ...good(), clean: 'yes' }, { ...good(), households: [{ ...good().households[0], outcome: 'failed' }] },
    { ...good(), households: [{ ...good().households[0], levels: [{ room: 'Room', volume: 101 }] }] }];
  for (const answer of invalid) {
    const { c, calls } = fixture({ answer }); await prepare(c);
    assert.equal(await c.confirm(), false); assert.equal(c.review(), false); assert.equal(await c.confirm(), false);
    assert.equal(calls.length, 3); assert.equal(c.state.report, null); assert.match(c.state.message, /not repeated/);
  }
});

test('partial playback and restoration outcomes retain applied levels and all details', async () => {
  for (const outcome of ['deadline', 'never_started', 'failed']) {
    const answer = good(); answer.clean = false;
    Object.assign(answer.households[0], { outcome, error: 'Player unavailable', not_restored: ['Room volume: user changed it'] });
    const { c } = fixture({ answer }); await prepare(c); assert.equal(await c.confirm(), true);
    assert.match(c.state.message, /did not complete cleanly/);
    assert.deepEqual(c.state.report.households[0].not_restored, ['Room volume: user changed it']);
    assert.equal(c.state.report.households[0].levels[0].volume, 20);
  }
});

test('late room reads cannot replace a newer read or resurrect a closed panel', async () => {
  for (const close of [false, true]) {
    const waiting = deferred(); let reads = 0;
    const { c } = fixture({ read: () => ++reads === 1 ? waiting.promise : [groups()[1]] });
    const old = c.refresh(); if (close) c.close(); else await c.refresh();
    waiting.resolve(groups()); assert.equal(await old, false);
    assert.equal(c.state.rooms.length, close ? 0 : 1);
  }
});

test('refresh drops vanished rooms without selecting replacements', async () => {
  let reads = 0; const { c } = fixture({ read: () => ++reads === 1 ? groups() : [groups()[0]] });
  await prepare(c); await c.refresh(); assert.equal(c.state.selected.size, 0); assert.equal(c.review(), false);
});

test('status refresh failures cannot turn an acknowledged announcement into a retry', async () => {
  for (const completed of [() => { throw new Error('read failed'); }, () => Promise.reject(new Error('read failed'))]) {
    const { c, calls } = fixture({ completed }); await prepare(c);
    assert.equal(await c.confirm(), true); await new Promise(setImmediate);
    assert.equal(calls.length, 3); assert.equal(c.state.report.clean, true);
  }
});

test('disposed controllers reject every mutation and late completion callbacks', async () => {
  let completed = 0; const waiting = deferred(); const { c, calls } = fixture({ answer: waiting.promise, completed: () => completed++ });
  await prepare(c); const work = c.confirm(); await new Promise(setImmediate); c.dispose();
  waiting.resolve(good()); await work;
  assert.equal(completed, 0); assert.equal(await c.refresh(), false); assert.equal(c.review(), false);
  assert.equal(c.field('words', 'Again'), false); assert.equal(c.select('Room@S2', true), false); assert.equal(calls.length, 3);
});

test('unknown model fields and sources cannot alter a request', async () => {
  const { c } = fixture(); await prepare(c);
  for (const field of ['__proto__', 'constructor', 'pending', 'rooms']) assert.equal(c.field(field, 'anything'), false);
  assert.equal(c.field('source', 'arbitrary'), false); assert.equal(c.select('Room@S2', 'false'), false);
});

test('the embedded assets and CI include the controller without weakening browser policy', () => {
  const root = path.join(__dirname, '..');
  const embedding = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.match(embedding, /include_str!\("\.\.\/remote\/announcements\.js"\)/);
  assert.match(embedding, /include_str!\("\.\.\/remote\/announcements\.css"\)/);
  assert.match(embedding, /MODE_SCRIPT}\\n\{ANNOUNCEMENTS_SCRIPT}/);
  assert.match(embedding, /connect-src 'self'/); assert.match(embedding, /frame-ancestors 'none'/);
  assert.match(workflow, /node --test scripts\/test-remote-announcements.cjs/);
  assert.deepEqual(Remote.routes.announceAudio, ['POST', '/announce']);
  for (const name of ['announcements.js', 'announcements.css']) {
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', name), 'utf8').toLowerCase();
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.ok(!source.includes(secret), `${name}: ${secret}`);
    }
  }
});

test('the controller completes speech through a real loopback JSON endpoint', async (t) => {
  const seen = [];
  const server = http.createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    seen.push({ method: request.method, url: request.url, type: request.headers['content-type'], body: Buffer.concat(chunks).toString() });
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(request.url === '/zones' ? groups() : good()));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  const c = Remote.model({ call: async (name, { body } = {}) => {
    const [method, pathname] = name === 'zones' ? ['GET', '/zones'] : Remote.routes[name];
    const response = await fetch(`http://127.0.0.1:${server.address().port}${pathname}`, {
      method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined,
    });
    return response.json();
  } });
  await prepare(c); assert.equal(await c.confirm(), true);
  assert.equal(seen.length, 3); assert.equal(seen[2].url, '/announce'); assert.equal(seen[2].method, 'POST');
  assert.deepEqual(JSON.parse(seen[2].body), { rooms: ['Room@S2'], volume: 35, text: 'Dinner is ready' });
});

function chunk(name, data) {
  const header = Buffer.alloc(8); header.write(name); header.writeUInt32LE(data.length, 4);
  return Buffer.concat([header, data, Buffer.alloc(data.length & 1)]);
}
function riff(...chunks) {
  const rest = Buffer.concat([Buffer.from('WAVE'), ...chunks]);
  const header = Buffer.alloc(8); header.write('RIFF'); header.writeUInt32LE(rest.length, 4);
  return Buffer.concat([header, rest]);
}
function format(rate = 16000, channels = 1, extensible = false) {
  const fmt = Buffer.alloc(extensible ? 40 : 16);
  fmt.writeUInt16LE(extensible ? 65534 : 1, 0); fmt.writeUInt16LE(channels, 2);
  fmt.writeUInt32LE(rate, 4); fmt.writeUInt32LE(rate * channels * 2, 8);
  fmt.writeUInt16LE(channels * 2, 12); fmt.writeUInt16LE(16, 14);
  if (extensible) {
    fmt.writeUInt16LE(22, 16); fmt.writeUInt16LE(16, 18);
    Buffer.from([1,0,0,0,0,0,16,0,128,0,0,170,0,56,155,113]).copy(fmt, 24);
  }
  return fmt;
}
const wav = (frames = 160, rate = 16000, channels = 1) => riff(chunk('fmt ', format(rate, channels)), chunk('data', Buffer.alloc(frames * channels * 2, 3)));
const file = (bytes, name = 'clip.wav') => ({ name, size: bytes.length, bytes });
const memoryRead = (f) => ({ promise: Promise.resolve(f.bytes.buffer.slice(f.bytes.byteOffset, f.bytes.byteOffset + f.bytes.byteLength)), cancel() {} });
async function wavReady(c, bytes = wav()) {
  await c.refresh(); c.select('Room@S2', true); c.field('source', 'wav');
  return c.chooseWav(file(bytes));
}

test('WAV validation accepts all nine backend sample rates in mono and stereo', () => {
  for (const rate of [8000,11025,12000,16000,22050,24000,32000,44100,48000]) {
    for (const channels of [1,2]) {
      const bytes = wav(rate, rate, channels); const info = Remote.wavInfo(bytes);
      assert.deepEqual(info, { size: bytes.length, channels, rate, seconds: 1 });
    }
  }
});

test('WAV metadata, padding, offset views and extensible PCM are supported', () => {
  for (const channels of [1,2]) {
    const bytes = riff(chunk('JUNK', Buffer.from('odd')), chunk('fmt ', format(24000, channels, true)),
      chunk('data', Buffer.alloc(channels * 2 * 24000)), chunk('LIST', Buffer.from('metadata')));
    const framed = Buffer.concat([Buffer.from('prefix'), bytes, Buffer.from('suffix')]);
    assert.equal(Remote.wavInfo(framed.subarray(6, 6 + bytes.length)).seconds, 1);
  }
});

test('every truncated prefix and inconsistent PCM field is rejected', () => {
  const bytes = wav();
  for (let end = 0; end < bytes.length; end++) assert.throws(() => Remote.wavInfo(bytes.subarray(0, end)), `prefix ${end}`);
  for (const [offset, value] of [[4,0],[20,3],[22,3],[24,0],[28,1],[32,1],[34,8],[40,0xffffffff]]) {
    const bad = Buffer.from(bytes);
    if ([20,22,32,34].includes(offset)) bad.writeUInt16LE(value,offset); else bad.writeUInt32LE(value,offset);
    assert.throws(() => Remote.wavInfo(bad), `offset ${offset}`);
  }
});

test('duplicate chunks, absent format, unaligned data, padding and trailing garbage fail closed', () => {
  const fmt = chunk('fmt ',format()), data = chunk('data',Buffer.alloc(4));
  for (const bytes of [riff(data,fmt),riff(fmt,fmt,data),riff(fmt,data,data),riff(fmt),riff(fmt,chunk('data',Buffer.alloc(0))),
    riff(fmt,chunk('data',Buffer.alloc(3))),riff(fmt,data,Buffer.from('bad')),riff(fmt,data,chunk('JUNK',Buffer.from('a')).subarray(0,9))]) {
    assert.throws(() => Remote.wavInfo(bytes));
  }
});

test('extensible GUID, valid bits and extension length must describe complete PCM', () => {
  for (const [offset,value] of [[16,21],[16,255],[18,8],[24,3]]) {
    const fmt = format(24000,1,true); fmt.writeUInt16LE(value,offset);
    assert.throws(() => Remote.wavInfo(riff(chunk('fmt ',fmt),chunk('data',Buffer.alloc(4)))));
  }
});

test('five minutes is accepted exactly and one additional frame is rejected', () => {
  assert.equal(Remote.wavInfo(wav(8000*300,8000)).seconds,300);
  assert.throws(() => Remote.wavInfo(wav(8000*300+1,8000)), /five minutes/);
});

test('the 16 MiB limit includes metadata and rejects a byte beyond it', () => {
  const data = wav();
  const filler = chunk('JUNK',Buffer.alloc(Remote.MAX_WAV_BYTES - data.length - 8));
  const bytes = riff(data.subarray(12),filler);
  assert.equal(Remote.wavInfo(bytes).size, Remote.MAX_WAV_BYTES);
  assert.throws(() => Remote.wavInfo(Buffer.alloc(Remote.MAX_WAV_BYTES+1)), /invalid size/);
});

test('WAV choice checks bytes locally and uploads exactly once only after review', async () => {
  const { c, calls } = fixture({ readFile: memoryRead }); const bytes = wav();
  assert.equal(await wavReady(c,bytes),true); assert.equal(calls.length,1);
  assert.equal(await c.confirm(),false); assert.equal(c.review(),true);
  assert.match(c.state.pending.label,/clip.wav/); assert.match(c.state.pending.label,/16000 Hz/);
  const pending = c.state.pending;
  assert.equal(await c.confirm(pending),true);
  assert.deepEqual(calls[2][1].body, { rooms:['Room@S2'],volume:35,wav_base64:bytes.toString('base64') });
  assert.equal(c.state.wav,null,'the consumed upload is released');
  assert.equal(await c.confirm(pending),false); assert.equal(calls.length,3);
});

test('base64 remains byte-exact around chunk boundaries and every padding case', async () => {
  for (const frames of [1,2,3,12264,12265,12266,12267,24578]) {
    const { c } = fixture({ readFile: memoryRead }); const bytes = wav(frames);
    await wavReady(c,bytes); c.review();
    assert.equal(c.state.pending.body.wav_base64,bytes.toString('base64'));
    assert.deepEqual(Buffer.from(c.state.pending.body.wav_base64,'base64'),bytes);
  }
});

test('oversized or unreadable selections clear an older clip without starting a file read', async () => {
  let reads=0; const { c,calls }=fixture({readFile:(f)=>{reads++; return memoryRead(f);}});
  await wavReady(c); c.review();
  for (const candidate of [null, {size:Remote.MAX_WAV_BYTES+1},{size:0},{size:1.5}]) {
    assert.equal(await c.chooseWav(candidate),false); assert.equal(c.state.wav,null); assert.equal(c.state.pending,null);
  }
  assert.equal(reads,1); assert.equal(calls.length,1);
});

test('a renamed non-WAV, changed size or read failure never authorizes upload', async () => {
  for (const readFile of [memoryRead,()=>({promise:Promise.resolve(new ArrayBuffer(4)),cancel(){}}),
    ()=>({promise:Promise.reject(new Error('File unavailable')),cancel(){}}),()=>{throw new Error('Read refused');}]) {
    const { c,calls }=fixture({readFile});
    assert.equal(await wavReady(c,Buffer.alloc(44)),false);
    assert.equal(c.review(),false); assert.equal(c.state.wav,null); assert.equal(calls.length,1);
  }
});

test('replacing a reviewed WAV invalidates its old confirmation and never uploads its filename', async () => {
  const { c,calls }=fixture({readFile:memoryRead}); await wavReady(c); c.review(); const old=c.state.pending;
  const bytes=wav(32); await c.chooseWav(file(bytes,'<img onerror=bad>.wav')); c.review();
  assert.equal(await c.confirm(old),false); assert.equal(await c.confirm(),true);
  assert.equal(calls[2][1].body.wav_base64,bytes.toString('base64'));
  assert.deepEqual(Object.keys(calls[2][1].body).sort(),['rooms','volume','wav_base64']);
});

test('changing source, clearing or closing discards WAV bytes and review consent', async () => {
  for (const action of [(c)=>c.field('source','chime'),(c)=>c.clearWav(),(c)=>c.close(),(c)=>c.dispose()]) {
    const { c,calls }=fixture({readFile:memoryRead}); await wavReady(c); c.review(); const old=c.state.pending;
    action(c); assert.equal(c.state.wav,null); assert.equal(c.state.pending,null); assert.equal(await c.confirm(old),false);
    assert.equal(calls.length,1);
  }
});

test('only one local file reader stays active and superseded completion is ignored', async () => {
  const waiting=deferred(); let cancelled=0, reads=0;
  const { c }=fixture({readFile:(f)=> ++reads===1
    ? {promise:waiting.promise,cancel(){cancelled++;}} : memoryRead(f)});
  await c.refresh(); c.select('Room@S2',true); c.field('source','wav');
  const first=c.chooseWav(file(wav(8),'old.wav'));
  assert.equal(c.state.fileLoading,true); assert.equal(c.review(),false);
  assert.equal(await c.chooseWav(file(wav(16),'new.wav')),true);
  waiting.resolve(wav(8).buffer); assert.equal(await first,false);
  assert.equal(cancelled,1); assert.equal(c.state.wav.name,'new.wav'); assert.equal(c.state.fileLoading,false);
});

test('closing or refreshing aborts pending file reads without uploading', async () => {
  for (const action of ['close','refresh','clearWav','dispose']) {
    const waiting=deferred(); let cancelled=0;
    const { c,calls }=fixture({readFile:()=>({promise:waiting.promise,cancel(){cancelled++;waiting.reject(new Error('cancelled'));}})});
    await c.refresh(); c.select('Room@S2',true); c.field('source','wav');
    const work=c.chooseWav(file(wav())); await c[action](); assert.equal(await work,false);
    assert.equal(cancelled,1); assert.equal(c.state.fileLoading,false); assert.equal(c.state.wav,null);
    assert.ok(calls.every(([route])=>route==='zones'));
  }
});

test('WAV selection and clearing cannot alter an already dispatched announcement', async () => {
  const waiting=deferred(); const { c,calls }=fixture({readFile:memoryRead,answer:waiting.promise});
  await wavReady(c); c.review(); const work=c.confirm(); await new Promise(setImmediate);
  assert.equal(await c.chooseWav(file(wav(10))),false); assert.equal(c.clearWav(),false);
  waiting.resolve(good()); assert.equal(await work,true); assert.equal(calls.length,3);
});

test('unsupported sample rates and compressed or float WAV files are not silently converted', () => {
  for (const rate of [0,7999,8100,47999,96000]) assert.throws(()=>Remote.wavInfo(wav(16,rate)));
  for (const code of [0,2,3,6,7,85]) {
    const fmt=format();fmt.writeUInt16LE(code);
    assert.throws(()=>Remote.wavInfo(riff(chunk('fmt ',fmt),chunk('data',Buffer.alloc(4)))));
  }
});

test('large room selections cannot exceed the WAV envelope metadata allowance', async () => {
  const members=Array.from({length:100},(_,i)=>`Room ${i} ${'x'.repeat(200)}`);
  const { c,calls }=fixture({read:[{household:'S2',coordinator_room:members[0],members}],readFile:memoryRead});
  await c.refresh(); for (const r of c.state.rooms) c.select(r.target,true);
  c.field('source','wav'); await c.chooseWav(file(wav()));
  assert.equal(c.review(),false); assert.match(c.state.message,/request budget/); assert.equal(calls.length,1);
});
