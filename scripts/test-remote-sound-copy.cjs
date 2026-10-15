// Run: node --test scripts/test-remote-sound-copy.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Copy = require('../crates/fsonos-api/src/remote/sound-copy.js');
const wanted = { bass: 4, treble: -2, loudness: false };
const flat = { bass: 0, treble: 0, loudness: true };
const zones = [{ household: 'S2', coordinator_room: 'First', members: ['First', 'Second', 'Third'] },
  { household: 'S1', coordinator_room: 'First', members: ['First'] }];
const tick = () => new Promise((r) => setImmediate(r));
function deferred() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
function fixture(options = {}) {
  let groups = options.zones || zones, hook = null;
  const tones = new Map(Copy.inventory(groups).map((r) => [r.target, { ...flat }]));
  tones.set('First@S2', { ...wanted });
  const calls = [];
  const normal = async (name, args) => {
    if (name === 'zones') return structuredClone(groups);
    const target = args.params?.room || args.body.zone;
    assert.ok(tones.has(target), target);
    if (name === 'soundCopyWrite') {
      const next = { ...tones.get(target) };
      for (const field of ['bass', 'treble', 'loudness']) if (field in args.body) next[field] = args.body[field];
      tones.set(target, next);
      return { done: 'Changed.', eq: { room: target.split('@')[0], ...next } };
    }
    assert.equal(name, 'soundCopyRead');
    return { room: target.split('@')[0], ...tones.get(target) };
  };
  const call = async (name, args) => { calls.push({ name, args: structuredClone(args) }); return hook ? hook(name, args, normal) : normal(name, args); };
  const model = Copy.model({ call, changed: options.changed });
  return { model, calls, tones, normal, call,
    hook: (f) => { hook = f; }, groups: (v) => { groups = v; },
    writes: () => calls.filter((c) => c.name === 'soundCopyWrite'),
    async ready(targets = ['Second@S2', 'Third@S2']) {
      await model.refresh(); model.selectSource('First@S2'); await model.readSource();
      for (const target of targets) assert.equal(model.selectTarget(target, true), true);
      const plan = await model.prepare(); assert.ok(plan); return plan;
    } };
}

test('inventory includes noncoordinators and separates the same name across households', () => {
  assert.deepEqual(Copy.inventory(zones).map((r) => r.target), ['First@S2', 'Second@S2', 'Third@S2', 'First@S1']);
  assert.ok(Object.isFrozen(Copy.inventory(zones)[0]));
});
test('ambiguous, duplicated and malformed inventories fail closed', () => {
  for (const value of [null, {}, [null], [{ ...zones[0], members: [] }],
    [{ ...zones[0], members: ['Second'] }], [{ ...zones[0], members: ['First', 'First'] }],
    [zones[0], zones[0]], [{ household: 'b@c', coordinator_room: 'a', members: ['a'] },
      { household: 'c', coordinator_room: 'a@b', members: ['a@b'] }]]) assert.throws(() => Copy.inventory(value));
});
test('tone reads require the correct room, bounded integer levels and boolean loudness', () => {
  const room = Copy.inventory(zones)[0];
  for (const value of [null, {}, { room: 'Second', ...flat }, { room: 'First', ...flat, bass: 11 },
    { room: 'First', ...flat, treble: -11 }, { room: 'First', ...flat, bass: '0' },
    { room: 'First', ...flat, bass: 0.5 }, { room: 'First', ...flat, loudness: 0 }]) {
    assert.throws(() => Copy.observed(value, room));
  }
});
test('nothing reads or selects speakers before opening, and no write happens while preparing', async () => {
  const f = fixture(); assert.deepEqual(f.calls, []);
  assert.equal(await f.model.prepare(), null); assert.equal(await f.model.confirm({}), false);
  const plan = await f.ready(); assert.equal(f.writes().length, 0);
  assert.deepEqual(plan.tone, wanted); assert.equal(plan.entries.length, 2);
  assert.ok(Object.isFrozen(plan)); assert.ok(Object.isFrozen(plan.entries));
  assert.throws(() => { plan.entries[0].patch.bass = 9; });
});
test('a complete copy writes each selected room once, reads back, and never writes its source', async () => {
  const f = fixture(); const plan = await f.ready(['Second@S2', 'First@S1']);
  assert.equal(await f.model.confirm(plan), true);
  assert.deepEqual(f.writes().map((c) => c.args.body), [
    { zone: 'Second@S2', ...wanted }, { zone: 'First@S1', ...wanted }]);
  assert.deepEqual(f.tones.get('Third@S2'), flat);
  assert.deepEqual(f.tones.get('First@S2'), wanted);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['verified', 'verified']);
  for (const row of f.model.view().report.rows) assert.deepEqual(row.observed, wanted);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 2);
});
test('patches preserve zero and false and omit destination fields already matching the source', async () => {
  const f = fixture(); f.tones.set('First@S2', { bass: 0, treble: -2, loudness: false });
  f.tones.set('Second@S2', { bass: 5, treble: -2, loudness: true });
  const plan = await f.ready(['Second@S2']); await f.model.confirm(plan);
  assert.deepEqual(f.writes()[0].args.body, { zone: 'Second@S2', bass: 0, loudness: false });
});
test('already-matching speakers get no mutation or redundant action-log entry', async () => {
  const f = fixture(); f.tones.set('Second@S2', { ...wanted });
  const plan = await f.ready(); assert.deepEqual(plan.entries[0].patch, {});
  assert.equal(await f.model.confirm(plan), true); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['unchanged', 'verified']);
});
test('selecting a source removes it from destinations and cannot copy to itself', async () => {
  const f = fixture(); await f.ready();
  assert.equal(f.model.selectTarget('First@S2', true), false);
  assert.equal(f.model.selectSource('Second@S2'), true);
  assert.deepEqual(f.model.view().targets, ['Third@S2']);
  assert.equal(f.model.view().sourceTone, null); assert.equal(f.model.view().review, null);
});
test('changing any destination consumes the old review', async () => {
  const f = fixture(); const plan = await f.ready();
  f.model.selectTarget('Third@S2', false);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.equal(await f.model.confirm({ ...await f.model.prepare() }), false);
});
test('an unread or malformed destination prevents a review instead of assuming defaults', async () => {
  const f = fixture(); await f.model.refresh(); f.model.selectSource('First@S2'); await f.model.readSource();
  f.model.selectTarget('Second@S2', true);
  f.hook((name, args, normal) => args?.params?.room === 'Second@S2' ? { room: 'Second', bass: 0 } : normal(name, args));
  assert.equal(await f.model.prepare(), null); assert.equal(f.model.view().canPrepare, false);
  assert.equal(f.writes().length, 0); assert.match(f.model.view().detail, /Second/);
});
test('source drift after review prevents every write', async () => {
  const f = fixture(); const plan = await f.ready(); f.tones.get('First@S2').bass = 7;
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.match(f.model.view().detail, /source sound changed/);
  assert.ok(f.model.view().report.rows.every((r) => r.status === 'not_attempted'));
});
test('all destinations are preflighted before the first write, even if the last one drifted', async () => {
  const f = fixture(); const plan = await f.ready(); f.tones.get('Third@S2').bass = 8;
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['not_attempted', 'conflict']);
});
test('a disappearing speaker stops the entire copy before mutation', async () => {
  const f = fixture(); const plan = await f.ready(); f.groups([{ ...zones[0], members: ['First', 'Second'] }]);
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 0);
  assert.match(f.model.view().detail, /Third.*no longer available/);
});
test('regrouping does not confuse per-room sound with the new coordinator', async () => {
  const f = fixture(); const plan = await f.ready();
  f.groups([{ ...zones[0], coordinator_room: 'Third' }, zones[1]]);
  assert.equal(await f.model.confirm(plan), true);
  assert.deepEqual(f.writes().map((c) => c.args.body.zone), ['Second@S2', 'Third@S2']);
});
test('a downstream change after the first command stops remaining writes without rollback', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const answer = await normal(name, args);
    if (name === 'soundCopyWrite') f.tones.get('Third@S2').treble = 9;
    return answer;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['verified', 'conflict']);
  assert.deepEqual(f.tones.get('Second@S2'), wanted);
});
test('lost replies stop immediately, preserve per-room uncertainty, and never automatically retry', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const value = await normal(name, args);
    if (name === 'soundCopyWrite') throw new Error('connection lost');
    return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['uncertain', 'not_attempted']);
  assert.equal(await f.model.confirm(plan), false);
  f.model.close(); f.model.resume(); await f.model.refresh(); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'uncertain');
  f.hook(null); f.model.selectSource('First@S2'); await f.model.readSource();
  const next = await f.model.prepare(); assert.ok(next);
  assert.equal(await f.model.confirm(next), true);
  assert.deepEqual(f.writes().map((c) => c.args.body.zone), ['Second@S2', 'Third@S2']);
});
test('an accepted command without readable verification does not allow the next speaker to change', async () => {
  const f = fixture(); const plan = await f.ready(); let sent = false;
  f.hook(async (name, args, normal) => {
    if (sent && name === 'soundCopyRead') throw new Error('read-back unavailable');
    const value = await normal(name, args); if (name === 'soundCopyWrite') sent = true; return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'accepted_unverified');
  assert.equal(f.model.view().report.rows[0].observed, null);
});
test('a successful acknowledgement with different actual settings is not presented as verified', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => {
    const value = await normal(name, args);
    if (name === 'soundCopyWrite') f.tones.set(args.body.zone, { ...flat });
    return value;
  });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  const row = f.model.view().report.rows[0]; assert.equal(row.status, 'different'); assert.deepEqual(row.observed, flat);
});
test('a malformed acknowledgement is uncertain even when the request took effect', async () => {
  const f = fixture(); const plan = await f.ready();
  f.hook(async (name, args, normal) => { const value = await normal(name, args); return name === 'soundCopyWrite' ? {} : value; });
  assert.equal(await f.model.confirm(plan), false); assert.equal(f.writes().length, 1);
  assert.equal(f.model.view().report.rows[0].status, 'uncertain');
});
test('double confirmation and edits are locked until the single in-flight command settles', async () => {
  const f = fixture(); const plan = await f.ready(); const gate = deferred();
  f.hook(async (name, args, normal) => name === 'soundCopyWrite' ? gate.promise : normal(name, args));
  const first = f.model.confirm(plan); await tick();
  assert.equal(f.writes().length, 1); assert.equal(await f.model.confirm(plan), false);
  assert.equal(f.model.selectTarget('First@S1', true), false); assert.equal(f.model.selectSource('Second@S2'), false);
  assert.equal(await f.model.readSource(), false); assert.equal(await f.model.refresh(), false);
  f.model.cancel(); gate.resolve({ done: 'accepted', eq: { room: 'Second', ...wanted } }); await first;
  assert.equal(f.writes().length, 1); assert.equal(f.model.view().writing, false);
  assert.deepEqual(f.model.view().report.rows.map((r) => r.status), ['accepted_unverified', 'not_attempted']);
});
for (const transition of ['cancel', 'close', 'suspend', 'dispose']) {
  test(`${transition} during preflight stops before any dispatch`, async () => {
    const f = fixture(); const plan = await f.ready(); const gate = deferred();
    f.hook((name, args, normal) => name === 'zones' ? gate.promise : normal(name, args));
    const pending = f.model.confirm(plan); f.model[transition](); gate.resolve(zones); await pending;
    assert.equal(f.writes().length, 0); assert.equal(f.model.view().writing, false);
  });
}
test('a cancellation requested from a progress callback is checked before dispatch', async () => {
  let model;
  const f = fixture({ changed: (state) => { if (state.report?.rows.some((r) => r.status === 'sending')) model.cancel(); } });
  model = f.model; const plan = await f.ready(); await model.confirm(plan);
  assert.equal(f.writes().length, 0); assert.equal(model.view().report.rows[0].status, 'not_sent');
});
test('view errors cannot turn accepted commands into retries', async () => {
  const f = fixture({ changed: () => { throw new Error('view detached'); } });
  assert.equal(await f.model.confirm(await f.ready()), true); assert.equal(f.writes().length, 2);
});
test('a stale source read cannot populate a newly selected source', async () => {
  const f = fixture(); await f.model.refresh(); f.model.selectSource('First@S2'); const gate = deferred();
  f.hook((name, args, normal) => args?.params?.room === 'First@S2' ? gate.promise : normal(name, args));
  const old = f.model.readSource(); f.model.selectSource('First@S1'); await f.model.readSource();
  gate.resolve({ room: 'First', ...wanted }); await old;
  assert.deepEqual(f.model.view().sourceTone, flat); assert.equal(f.model.view().source, 'First@S1');
});
test('destination count and read concurrency are bounded', async () => {
  const many = [{ household: 'S2', coordinator_room: 'First', members: ['First', ...Array.from({ length: 34 }, (_, i) => `Room${i}`)] }];
  const f = fixture({ zones: many }); await f.model.refresh(); f.model.selectSource('First@S2'); await f.model.readSource();
  for (let i = 0; i < Copy.MAX_TARGETS; ++i) assert.equal(f.model.selectTarget(`Room${i}@S2`, true), true);
  assert.equal(f.model.selectTarget('Room33@S2', true), false);
  let running = 0, peak = 0;
  f.hook(async (name, args, normal) => {
    if (name !== 'soundCopyRead') return normal(name, args);
    peak = Math.max(peak, ++running); await tick(); const value = await normal(name, args); --running; return value;
  });
  assert.ok(await f.model.prepare()); assert.equal(peak, Copy.READ_CONCURRENCY); assert.equal(running, 0);
  assert.equal(f.writes().length, 0);
});

// Exercise production controls and lifecycle listeners without extra dependencies.
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.events = new Map(); this.ownText = ''; this.disabled = this.hidden = this.open = this.checked = false; }
  setAttribute(k, v) { this.attrs[k] = String(v); if (['disabled', 'hidden', 'open', 'checked'].includes(k)) this[k] = true; if (k === 'value') this.value = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  addEventListener(k, f) { if (!this.events.has(k)) this.events.set(k, new Set()); this.events.get(k).add(f); }
  removeEventListener(k, f) { this.events.get(k)?.delete(f); }
  event(k) { for (const f of this.events.get(k) || []) f({ target: this }); }
  click() { if (!this.disabled) this.event('click'); }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = [...nodes]; this.ownText = ''; }
  set textContent(s) { this.ownText = String(s); this.children = []; }
  get textContent() { return this.ownText + this.children.map((c) => typeof c === 'string' ? c : c.textContent).join(''); }
  contains(node) { return all(this).includes(node); }
  querySelectorAll(selector) { const key = /^\[(.+)\]$/.exec(selector)?.[1]; return all(this).filter((n) => key in n.attrs); }
  focus() { this.focused = true; }
}
function el(tag, attrs = {}, ...children) {
  const node = new Element(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : String(value));
  }
  node.append(...children.filter((c) => c != null)); return node;
}
function all(node) { return [node, ...node.children.filter((n) => typeof n !== 'string').flatMap(all)]; }
function button(panel, label) { const n = all(panel.node).find((n) => n.tag === 'button' && n.textContent === label); assert.ok(n, label); return n; }
function labelled(panel, label) { const n = all(panel.node).find((n) => n.attrs['aria-label'] === label); assert.ok(n, label); return n; }
function panelFixture() {
  const f = fixture(), page = new Element('document'), browser = new Element('window');
  const panel = Copy.panel({ call: f.call, el, page, browser });
  return { ...f, panel, page, browser,
    async open() { const details = all(panel.node).find((n) => n.tag === 'details'); details.open = true; details.event('toggle'); await tick(); },
    async source() { const input = labelled(panel, 'Copy sound from'); input.value = 'First@S2'; input.event('change'); await tick(); },
    target(label, selected = true) { const input = labelled(panel, `Copy to ${label}`); input.checked = selected; input.event('change'); } };
}
test('the production panel is lazy, selection-only until review and explicit confirmation', async () => {
  const f = panelFixture(); assert.equal(f.calls.length, 0); await f.open();
  assert.equal(f.panel.view().source, ''); assert.deepEqual(f.panel.view().targets, []);
  await f.source(); f.target('Second (S2)'); f.target('First (S1)');
  assert.equal(f.writes().length, 0); button(f.panel, 'Review sound copy').click(); await tick();
  assert.ok(f.panel.node.textContent.includes('Before: bass 0, treble 0, loudness on'));
  assert.ok(f.panel.node.textContent.includes('After: bass +4, treble -2, loudness off'));
  assert.equal(f.writes().length, 0); button(f.panel, 'Confirm copy to 2 speakers').click(); await tick();
  assert.deepEqual(f.writes().map((c) => c.args.body.zone), ['Second@S2', 'First@S1']);
  assert.deepEqual(f.tones.get('Third@S2'), flat);
  assert.ok(f.panel.node.textContent.includes('Second (S2): Verified'));
  assert.ok(f.panel.node.textContent.includes('one undo does not reverse the whole copy'));
  f.panel.dispose();
});
test('a detached confirmation button cannot apply a replaced destination review', async () => {
  const f = panelFixture(); await f.open(); await f.source(); f.target('Second (S2)');
  button(f.panel, 'Review sound copy').click(); await tick();
  const old = button(f.panel, 'Confirm copy to 1 speakers'); f.target('Third (S2)');
  old.click(); await tick(); assert.equal(f.writes().length, 0);
  button(f.panel, 'Review sound copy').click(); await tick();
  button(f.panel, 'Cancel sound copy review').click(); old.click(); await tick();
  assert.equal(f.writes().length, 0); assert.equal(f.panel.view().review, null);
});
test('the actual stop button preserves an in-flight outcome and leaves other rooms alone', async () => {
  const f = panelFixture(); await f.open(); await f.source(); f.target('Second (S2)'); f.target('Third (S2)');
  button(f.panel, 'Review sound copy').click(); await tick(); const gate = deferred();
  f.hook(async (name, args, normal) => name === 'soundCopyWrite' ? gate.promise : normal(name, args));
  button(f.panel, 'Confirm copy to 2 speakers').click(); await tick();
  assert.equal(labelled(f.panel, 'Copy sound from').disabled, true);
  button(f.panel, 'Stop remaining copies').click(); gate.reject(new Error('reply lost')); await tick();
  assert.equal(f.writes().length, 1); assert.deepEqual(f.tones.get('Third@S2'), flat);
  assert.ok(f.panel.node.textContent.includes('Second (S2): Outcome uncertain'));
  assert.ok(f.panel.node.textContent.includes('Third (S2): Not attempted'));
  assert.equal(button(f.panel, 'Review sound copy').disabled, true);
});
test('page lifecycle invalidates live reviews and does not resume commands or automatic reads', async () => {
  const f = panelFixture(); await f.open(); await f.source(); f.target('Second (S2)');
  button(f.panel, 'Review sound copy').click(); await tick(); const old = button(f.panel, 'Confirm copy to 1 speakers');
  const count = f.calls.length;
  f.page.hidden = true; f.page.event('visibilitychange');
  assert.equal(labelled(f.panel, 'Copy sound from').disabled, true);
  f.page.hidden = false; f.page.event('visibilitychange'); old.click(); await tick();
  assert.equal(f.calls.length, count); assert.equal(f.writes().length, 0);
  assert.equal(button(f.panel, 'Review sound copy').disabled, true);
  button(f.panel, 'Read source sound').click(); await tick();
  assert.equal(button(f.panel, 'Review sound copy').disabled, false);
  f.panel.dispose();
  assert.equal(f.page.events.get('visibilitychange').size, 0);
  assert.equal(f.browser.events.get('pagehide').size, 0); assert.equal(f.browser.events.get('pageshow').size, 0);
});
test('room names render as text, not markup, in source options and destinations', async () => {
  const f = panelFixture(); f.groups([{ household: 'S2', coordinator_room: 'First', members: ['First', '<img src=x onerror=bad()>'] }]);
  await f.open(); assert.ok(f.panel.node.textContent.includes('<img src=x onerror=bad()>'));
  assert.ok(!all(f.panel.node).some((n) => n.tag === 'img'));
});

for (const loseReply of [false, true]) {
  test(`real HTTP copy: ${loseReply ? 'lost POST response stops later destinations' : 'qualified partial writes and read-back'}`, async (t) => {
    const http = require('node:http'); const f = fixture(), requests = [];
    const server = http.createServer(async (req, res) => {
      try {
        requests.push({ method: req.method, path: req.url });
        let value;
        if (req.method === 'POST') {
          assert.equal(req.url, '/eq'); assert.equal(req.headers['content-type'], 'application/json');
          let body = ''; for await (const part of req) body += part;
          value = await f.call('soundCopyWrite', { body: JSON.parse(body) });
          if (loseReply) { req.socket.destroy(); return; }
        } else if (req.url === '/zones') value = await f.call('zones');
        else {
          const match = /^\/zones\/([^/]+)\/eq$/.exec(req.url); assert.ok(match);
          value = await f.call('soundCopyRead', { params: { room: decodeURIComponent(match[1]) } });
        }
        res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(value));
      } catch (error) { res.statusCode = 500; res.end(JSON.stringify({ error: error.message })); }
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    t.after(() => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }));
    const origin = `http://127.0.0.1:${server.address().port}`;
    const model = Copy.model({ call: async (name, args = {}) => {
      const [method, route] = name === 'zones' ? ['GET', '/zones'] : Copy.routes[name];
      const path = route.replace('{room}', encodeURIComponent(args.params?.room));
      const response = await fetch(origin + path, { method, signal: AbortSignal.timeout(2000),
        headers: { 'content-type': 'application/json' }, ...(args.body ? { body: JSON.stringify(args.body) } : {}) });
      assert.equal(response.status, 200); return response.json();
    } });
    // The first destination already shares treble: it must not enter the patch.
    f.tones.get('Second@S2').treble = wanted.treble;
    await model.refresh(); model.selectSource('First@S2'); await model.readSource();
    model.selectTarget('Second@S2', true); model.selectTarget('Third@S2', true);
    const plan = await model.prepare(); assert.equal(f.writes().length, 0);
    assert.equal(await model.confirm(plan), !loseReply);
    assert.deepEqual(f.writes()[0].args.body, { zone: 'Second@S2', bass: 4, loudness: false });
    assert.equal(f.writes().length, loseReply ? 1 : 2);
    assert.deepEqual(model.view().report.rows.map((r) => r.status), loseReply ? ['uncertain', 'not_attempted'] : ['verified', 'verified']);
    assert.ok(requests.some((r) => r.path === '/zones/Second%40S2/eq'));
    if (loseReply) {
      assert.deepEqual(f.tones.get('Third@S2'), flat);
      assert.equal(await model.confirm(plan), false); await model.readSource();
      assert.equal(f.writes().length, 1);
    }
  });
}

test('the daemon embeds both copy assets, preserves the single-speaker editor, and checks asset privacy', () => {
  const fs = require('node:fs'), path = require('node:path');
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const rust = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  for (const [file, constant] of [['sound-copy.js', 'SOUND_COPY_SCRIPT'], ['sound-copy.css', 'SOUND_COPY_STYLE']]) {
    assert.ok(rust.includes(`const ${constant}: &str = include_str!("../remote/${file}");`));
    assert.ok(rust.includes(`\\n{${constant}}`)); assert.ok(rust.includes(`("${file}", ${constant})`));
    const source = fs.readFileSync(path.join(root, 'remote', file), 'utf8').toLowerCase();
    for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
      assert.equal(source.includes(secret), false, `${file}: ${secret}`);
    }
    assert.doesNotMatch(source, /innerhtml|outerhtml|insertadjacenthtml|\beval\(/);
  }
  assert.ok(rust.includes('../remote/equalizer.js'));
  assert.ok(rust.includes('{EQ_SCRIPT}\\n{SOUND_COPY_SCRIPT}'));
});
