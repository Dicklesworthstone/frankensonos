'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const R = require('../crates/fsonos-api/src/remote/favorite-routines.js');
const copy = (v) => JSON.parse(JSON.stringify(v));
const ID = '0123456789ABCDEFGHIJKL';
const nonce = () => 'abcdef0123456789abcdef0123456789';
const zones = [{ household: 'S2', coordinator_room: 'Kitchen', members: ['Kitchen', 'Study'] }];
const group = R.groupsOf(zones)[0];
const favorite = { id: 'FV:2/1', title: 'Morning', kind: 'container', description: null };
const draft = (extra = {}) => ({ label: 'Morning music', cadence: 'weekdays', time: '07:30',
  source_kind: 'spotify', source_uri: `spotify:album:${ID}`, ...extra });
const journal = () => {
  let value = null;
  return { load: () => copy(value), save: (v) => { value = copy(v); }, clear: () => { value = null; } };
};
function backend() {
  const b = { zones: copy(zones), scenes: [], rows: [], calls: [], intercept: null };
  b.call = async (name, args = {}) => {
    b.calls.push([name, copy(args)]);
    if (b.intercept) { const v = await b.intercept(name, args); if (v !== undefined) return v; }
    switch (name) {
      case 'routineZones': return copy(b.zones);
      case 'routineFavorites': throw new Error('Favorites are unavailable; URI routines must not depend on them.');
      case 'routineScenes': return copy(b.scenes);
      case 'routineSchedules': return copy(b.rows);
      case 'routineSaveScene': {
        const s = { name: args.params.name, saved: 123, ...copy(args.body) };
        b.scenes.push(s); return copy(s);
      }
      case 'routineAdd': {
        const r = { id: 1, when: args.body.when, scene: args.body.scene, action: 'apply_scene',
          enabled: true, creator: 'loopback-http', summary: 'Apply music scene', next: '2026-10-16T07:30:00Z' };
        b.rows.push(r); return copy(r);
      }
      default: throw new Error(`Unexpected route ${name}`);
    }
  };
  return b;
}
const writes = (b) => b.calls.filter(([name]) => ['routineSaveScene', 'routineAdd'].includes(name));
async function ready(input = draft(), b = backend(), options = {}) {
  const m = R.model({ call: b.call, nonce, ...options });
  await m.refresh(); await m.choose(group.target, input.source_kind); m.review(input);
  assert.ok(m.state.pending, m.state.message);
  return { b, m, p: m.state.pending };
}

for (const kind of ['track', 'album', 'playlist', 'artist', 'episode', 'show']) {
  test(`Spotify ${kind} canonicalization uses the backend's source vocabulary`, () => {
    const expected = { kind: 'uri', uri: `spotify:${kind}:${ID}` };
    for (const source_uri of [`spotify:${kind}:${ID}`, `Spotify:${kind.toUpperCase()}:${ID}`,
      `https://open.spotify.com/${kind}/${ID}?si=tracking`,
      `https://open.spotify.com/intl-de/embed/${kind}/${ID}/#share`, `http://play.spotify.com/${kind}/${ID}`]) {
      assert.deepEqual(R.sourceOf(draft({ source_uri })), expected);
    }
  });
  test(`Spotify ${kind} routine persists scene and schedule without favorite discovery`, async () => {
    const { b, m, p } = await ready(draft({ source_uri: `spotify:${kind}:${ID}` }));
    assert.equal(writes(b).length, 0);
    await m.confirm(p); await m.confirm(p);
    assert.equal(m.state.phase, 'complete', m.state.message);
    assert.deepEqual(writes(b).map(([name]) => name), ['routineSaveScene', 'routineAdd']);
    assert.equal(b.calls.some(([name]) => name === 'routineFavorites'), false);
    assert.deepEqual(b.scenes[0].groups[0].source, { kind: 'uri', uri: `spotify:${kind}:${ID}` });
    assert.deepEqual(b.scenes[0].volumes, {}); assert.deepEqual(b.scenes[0].mutes, {});
  });
}

test('legacy Spotify playlists use canonical playlist identity', () => {
  for (const source_uri of [`spotify:user:listener:playlist:${ID}`, `https://open.spotify.com/user/listener/playlist/${ID}`]) {
    assert.equal(R.sourceOf(draft({ source_uri })).uri, `spotify:playlist:${ID}`);
  }
});

for (const uri of ['https://radio.example.invalid/live.mp3?channel=2', 'http://192.0.2.44:8080/audio.aac']) {
  test(`stream routine preserves the exact reviewed URI: ${uri}`, async () => {
    const { b, m, p } = await ready(draft({ source_kind: 'stream', source_uri: uri }));
    assert.equal(p.source.uri, uri); await m.confirm(p);
    assert.equal(m.state.phase, 'complete', m.state.message);
    assert.equal(b.scenes[0].groups[0].source.uri, uri);
    assert.equal(b.calls.some(([name]) => name === 'routineFavorites'), false);
  });
}

const invalid = [
  ['spotify', 'spotify:album:short'], ['spotify', `spotify:genre:${ID}`], ['spotify', `spotify:local:${ID}`],
  ['spotify', `https://open.spotify.com.evil.invalid/album/${ID}`],
  ['spotify', `https://user@open.spotify.com/album/${ID}`], ['spotify', `https://open.spotify.com:443/album/${ID}`],
  ['spotify', `https://open.spotify.com/not/../album/${ID}`], ['spotify', 'https://spotify.link/short'],
  ['spotify', `spotify:album:${ID}\nmore`], ['spotify', `spotify:album:${'A'.repeat(23)}`],
  ['stream', 'javascript:alert(1)'], ['stream', 'file:///tmp/audio.mp3'], ['stream', '//radio.example.invalid/live'],
  ['stream', 'https://listener:secret@radio.example.invalid/live'], ['stream', 'https://radio.example.invalid/live#fragment'],
  ['stream', 'https://radio.example.invalid/white space'], ['stream', 'https://radio.example.invalid/a\\b'],
  ['stream', `https://open.spotify.com/album/${ID}`], ['stream', 'https://spotify.link/short'],
  ['stream', `https://radio.example.invalid/${'é'.repeat(1024)}`],
];
for (const [source_kind, source_uri] of invalid) test(`reject invalid ${source_kind} source ${invalid.findIndex((r) => r[1] === source_uri) + 1} before mutation`, async () => {
  const b = backend(), m = R.model({ call: b.call, nonce });
  await m.refresh(); await m.choose(group.target, source_kind); m.review(draft({ source_kind, source_uri }));
  assert.equal(m.state.pending, null); assert.ok(m.state.message); assert.equal(writes(b).length, 0);
});

test('optional volume and unmute apply to exactly the reviewed household-qualified rooms', async () => {
  const { b, m, p } = await ready(draft({ volume: '23', unmute: true }));
  assert.deepEqual(p.definition.volumes, { 'Kitchen@S2': 23, 'Study@S2': 23 });
  assert.deepEqual(p.definition.mutes, { 'Kitchen@S2': false, 'Study@S2': false });
  assert.throws(() => { p.settings.volume = 90; }, TypeError);
  await m.confirm(p); assert.equal(m.state.phase, 'complete', m.state.message);
  assert.deepEqual(b.scenes[0].volumes, p.definition.volumes);
});

test('zero is a deliberate volume; unmute is independently opt-in', () => {
  const p = R.planOf(group, null, draft({ volume: '0' }), nonce());
  assert.deepEqual(p.definition.volumes, { 'Kitchen@S2': 0, 'Study@S2': 0 });
  assert.deepEqual(p.definition.mutes, {});
  for (const volume of ['101', '-1', '2.5', '1e1', 'Infinity', ' 20', 20]) assert.throws(() => R.settingsOf({ volume }));
  assert.throws(() => R.settingsOf({ unmute: 'true' }));
});

for (const property of ['volumes', 'mutes', 'source']) test(`changed ${property} in a saved acknowledgement cannot authorize the schedule`, async () => {
  const { b, m, p } = await ready(draft({ volume: '23', unmute: true }));
  b.intercept = async (name, args) => {
    if (name !== 'routineSaveScene') return;
    const s = { name: args.params.name, saved: 123, ...copy(args.body) };
    if (property === 'source') s.groups[0].source.uri = `spotify:track:${ID}`;
    else if (property === 'volumes') s.volumes['Kitchen@S2'] = 99;
    else s.mutes['Study@S2'] = true;
    b.scenes.push(s); return copy(s);
  };
  await m.confirm(p); assert.equal(writes(b).length, 1); assert.equal(m.state.phase, 'unknown');
});

for (const route of ['routineSaveScene', 'routineAdd']) test(`lost ${route} reply recovers without replay after reload`, async () => {
  const b = backend(), saved = journal(), raw = b.call;
  b.call = async (...args) => { const result = await raw(...args); if (args[0] === route) throw new Error('lost reply'); return result; };
  const { m, p } = await ready(draft({ volume: '20', unmute: true }), b, { journal: saved });
  await m.confirm(p); assert.equal(m.state.phase, 'unknown'); assert.equal(saved.load().version, 2);
  const reload = R.model({ call: b.call, nonce, journal: saved });
  const sent = writes(b).length; await reload.recover(); await reload.recover(); assert.equal(writes(b).length, sent);
  if (route === 'routineSaveScene') {
    assert.equal(reload.state.phase, 'scene_ready', reload.state.message);
    reload.reviewContinue(); await reload.continue(reload.state.pending);
    assert.equal(writes(b).length, 2); assert.equal(reload.state.phase, 'complete');
  } else assert.equal(reload.state.phase, 'complete', reload.state.message);
});

test('URI preflight rejects regrouping, and leaving during it dispatches nothing', async () => {
  const { b, m, p } = await ready(); b.zones[0].members = ['Kitchen']; await m.confirm(p); assert.equal(writes(b).length, 0);
  const next = await ready(); let release;
  next.b.intercept = async (name) => { if (name === 'routineZones') return new Promise((resolve) => { release = resolve; }); };
  const running = next.m.confirm(next.p); next.m.close(); release(copy(zones)); await running;
  assert.equal(writes(next.b).length, 0);
});

test('version-one favorite setup can still be inspected and explicitly continued', async () => {
  const p = copy(R.planOf(group, favorite, draft({ source_kind: 'favorite' }), nonce()));
  delete p.source; delete p.settings;
  const scene = { name: p.name, saved: 123, ...copy(p.definition) };
  scene.groups[0].source = { kind: 'favorite', name: favorite.title, uri: 'x-rincon-cpcontainer:fixture' };
  const a = { version: 1, plan: p, sceneSent: true, scheduleSent: false, scene: R.sceneOf(scene, p) };
  const saved = journal(); saved.save(a); const b = backend(); b.scenes.push(scene);
  b.intercept = async (name) => name === 'routineFavorites' ? [favorite] : undefined;
  const m = R.model({ call: b.call, nonce, journal: saved });
  await m.recover(); assert.equal(m.state.phase, 'scene_ready', m.state.message);
  assert.equal(writes(b).length, 0); m.reviewContinue(); await m.continue(m.state.pending);
  assert.equal(m.state.phase, 'complete', m.state.message); assert.equal(writes(b).length, 1);
});

for (const mutate of [
  (a) => { a.plan.source.uri = `spotify:track:${ID}`; },
  (a) => { a.plan.settings.volume = 90; },
  (a) => { a.plan.definition.groups[0].source.uri = 'javascript:alert(1)'; },
  (a) => { a.version = 1; },
]) test('inconsistent version-two recovery cannot create another routine', async () => {
  const a = { version: 2, plan: copy(R.planOf(group, null, draft(), nonce())), sceneSent: true, scheduleSent: false, scene: null };
  mutate(a); const saved = journal(); saved.save(a); const b = backend(), m = R.model({ call: b.call, nonce, journal: saved });
  await m.refresh(); await m.recover(); m.review(draft()); assert.equal(m.state.phase, 'blocked'); assert.equal(writes(b).length, 0);
});

test('real HTTP fixture receives only existing scene/schedule writes for Spotify with levels', async (t) => {
  const b = backend(), received = [];
  const server = http.createServer(async (req, res) => {
    try {
      let raw = ''; for await (const chunk of req) raw += chunk;
      const url = new URL(req.url, 'http://localhost'), body = raw ? JSON.parse(raw) : undefined;
      received.push([req.method, url.pathname]); let value;
      if (url.pathname === '/zones') value = await b.call('routineZones');
      else if (url.pathname === '/scenes') value = await b.call('routineScenes');
      else if (url.pathname.startsWith('/scenes/') && req.method === 'PUT') {
        assert.equal(req.headers['content-type'], 'application/json');
        value = await b.call('routineSaveScene', { params: { name: decodeURIComponent(url.pathname.slice(8)) }, body });
      } else if (url.pathname === '/schedules') value = await b.call(req.method === 'POST' ? 'routineAdd' : 'routineSchedules', { body });
      else throw new Error('Unexpected endpoint');
      res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(value));
    } catch (error) { res.writeHead(500); res.end(error.message); }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const call = async (name, { body, params = {} } = {}) => {
    const [method, route] = R.routes[name], path = route.replace(/\{(\w+)\}/g, (_, key) => encodeURIComponent(params[key]));
    const reply = await fetch(`http://127.0.0.1:${server.address().port}${path}`, { method,
      headers: { 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
    assert.equal(reply.status, 200); return reply.json();
  };
  const { m, p } = await ready(draft({ volume: '18', unmute: true }), b, { call }); await m.confirm(p);
  assert.equal(m.state.phase, 'complete', m.state.message);
  assert.equal(received.filter(([method]) => method === 'PUT').length, 1);
  assert.equal(received.filter(([method]) => method === 'POST').length, 1);
  assert.equal(received.some(([, path]) => path === '/favorites' || path.startsWith('/play')), false);
});
