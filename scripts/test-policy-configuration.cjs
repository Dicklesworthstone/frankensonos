'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const M = require('../crates/fsonos-api/src/remote/policy-configuration.js');
const P = require('../crates/fsonos-api/src/remote/policy-configuration-panel.js');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Studio = require('../crates/fsonos-api/src/remote/studio.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve; const promise = new Promise(yes => { resolve = yes; }); return { promise, resolve }; };
async function settled(predicate) {
  for (let n = 0; n < 300 && !predicate(); n++) await new Promise(setImmediate);
  assert.ok(predicate(), 'expected asynchronous boundary was reached');
}
function configuration() {
  return { revision: 'policy-review-A', defaults: { max_volume: 70, max_step: 20, fade_secs: 0 },
    quiet_hours: { start: '22:00', end: '07:00', max_volume: 25 },
    rooms: [{ room: 'Offline cabin', max_volume: 40 }, { room: 'Owner’s study', max_step: 7 }],
    clients: [{ client: 'agent@example.test', allow: ['read', 'playback', 'set_volume'], deny: ['dj'], grant: ['auth'],
      capped: true, manage_others: true, mcp_profile: 'full' }, { client: '__proto__', allow: [], deny: [] }] };
}
function documentFixture() {
  const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], attributes: {}, value: '', disabled: false, readOnly: false, open: false,
      _text: '', get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.children = []; },
      set innerHTML(_) { throw new Error('Policy names must remain text'); },
      setAttribute(name, value) { this.attributes[name] = String(value); if (['id', 'type'].includes(name)) this[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; }, append(...children) { this.children.push(...children); },
      replaceChildren(...children) { this._text = ''; this.children = children; },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(node => node.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); }, focus() { doc.activeElement = this; },
      setSelectionRange(start, end = start) { this.selectionStart = start; this.selectionEnd = end; } };
  } }; return doc;
}
function fixture(surface, { snapshot = configuration(), dom = false, demo = false, network } = {}) {
  let config = copy(snapshot), reader = null, writer = null, control;
  const calls = [];
  const read = url => {
    if (url === '/policy/configuration') return copy(config);
    if (url === '/zones' || url === '/schedules') return [];
    throw new Error('Unexpected read: ' + url);
  };
  const write = (url, body) => {
    assert.equal(url, '/policy/configuration'); assert.equal(body.expected_revision, config.revision);
    const before = copy(config), change = copy(body.change), kind = change.kind; delete change.kind;
    switch (kind) {
      case 'set_defaults': config.defaults = change; break;
      case 'set_quiet_hours': config.quiet_hours = change; break;
      case 'clear_quiet_hours': delete config.quiet_hours; break;
      case 'set_room': case 'remove_room': {
        const at = config.rooms.findIndex(row => row.room.toLowerCase() === change.room.toLowerCase());
        if (at >= 0) config.rooms.splice(at, 1);
        if (kind === 'set_room') config.rooms.push(change);
        break;
      }
      case 'set_client': case 'remove_client': {
        const at = config.clients.findIndex(row => row.client.toLowerCase() === change.client.toLowerCase());
        if (at >= 0) config.clients.splice(at, 1);
        if (kind === 'set_client') config.clients.push({ ...change, client: change.client.toLowerCase() });
        break;
      }
      default: throw new Error('Unexpected change');
    }
    const changed = JSON.stringify(config) !== JSON.stringify(before);
    if (changed) config.revision += '.next';
    return { done: changed ? 'Saved synthetic house policy.' : 'House policy already matched.', changed, configuration: copy(config) };
  };
  const raw = async (url, body, options) => {
    calls.push({ url, body: copy(body), options });
    return body === undefined ? reader ? reader(url, options, read) : read(url) : writer ? writer(url, body, write) : write(url, body);
  };
  const call = network ? network(raw) : raw;
  const controller = surface === 'dashboard' ? C.model({ call, demo, changed: () => control?.observe() }) : null;
  const options = controller ? { call, canEdit: () => controller.canManage(), canRead: () => controller.state.active && !controller.state.busy,
    blockedReason: () => demo ? 'Design preview is read-only.' : controller.state.message,
    readConfiguration: (decode, opts) => controller.refreshStored('policyConfiguration', decode, opts),
    execute: operation => controller.mutate(operation, { stored: 'policyConfiguration' }) } : { call, canEdit: () => !demo };
  const document = dom ? documentFixture() : null, panel = dom ? P.panel({ ...options, document }) : null;
  control = panel ? panel.control : M.model(options);
  return { surface, control, controller, panel, document, calls, call, raw,
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, setConfig(value) { config = copy(value); }, config: () => copy(config),
    writes: () => calls.filter(call => call.body !== undefined),
    async ready() { assert.equal(control.open(), true); await settled(() => !control.state.loading); assert.equal(control.state.loaded, true, control.state.message); return this; } };
}
function review(f, kind, fields = {}, key = null) {
  assert.equal(f.control.begin(kind, key), true);
  for (const [field, value] of Object.entries(fields)) assert.equal(f.control.edit(field, value), true);
  const planned = f.control.review(); assert.ok(planned, f.control.state.message); return planned;
}
async function reload(f) { assert.equal(await f.control.load({ explicit: true }), true, f.control.state.message); }

for (const surface of ['phone', 'dashboard']) {
  test(`${surface}: policy opens lazily without inventory, playback, or a write`, async () => {
    const f = fixture(surface); assert.deepEqual(f.calls, []); await f.ready();
    assert.deepEqual(f.calls.map(call => call.url), ['/policy/configuration']);
    assert.equal(f.control.state.configuration.clients[1].client, '__proto__');
    assert.deepEqual(f.control.state.configuration.clients[1].allow, []);
    assert.ok(Object.isFrozen(f.control.state.configuration.clients[0].grant));
    assert.equal(f.writes().length, 0); if (f.controller) assert.equal(f.controller.canControl(), false);
  });

  test(`${surface}: changes defaults, quiet hours, and offline room limits with one review per rule`, async () => {
    const f = await fixture(surface).ready();
    const first = review(f, 'set_defaults', { max_volume: '55', max_step: '9', fade_secs: '4' });
    assert.deepEqual(first.body.change, { kind: 'set_defaults', max_volume: 55, max_step: 9, fade_secs: 4 });
    assert.equal(f.writes().length, 0); assert.equal(await f.control.save(first), true); assert.equal(await f.control.save(first), false);
    await reload(f);
    assert.equal(await f.control.save(review(f, 'set_quiet_hours', { start: '23:30', end: '06:15', max_volume: '18' })), true);
    assert.deepEqual(f.config().quiet_hours, { start: '23:30', end: '06:15', max_volume: 18 });
    await reload(f);
    assert.equal(await f.control.save(review(f, 'set_room', { max_volume: '25', max_step: '' }, 'Offline cabin')), true);
    assert.deepEqual(f.config().rooms.find(row => row.room === 'Offline cabin'), { room: 'Offline cabin', max_volume: 25, max_step: null });
    assert.deepEqual(f.config().clients, configuration().clients);
    await reload(f); assert.equal(await f.control.save(review(f, 'remove_room', {}, 'Offline cabin')), true);
    await reload(f); assert.equal(await f.control.save(review(f, 'clear_quiet_hours')), true);
    assert.equal(f.config().quiet_hours, undefined); assert.equal(f.writes().length, 5);
    assert.ok(f.calls.every(call => call.url === '/policy/configuration'));
  });

  test(`${surface}: full client edits retain grants, admin flag, and MCP profile; empty allow is distinct from inherited`, async () => {
    const f = await fixture(surface).ready();
    const planned = review(f, 'set_client', { capped: 'uncapped' }, 'agent@example.test');
    assert.deepEqual(planned.body.change, { kind: 'set_client', ...configuration().clients[0], capped: false });
    assert.equal(await f.control.save(planned), true);
    await reload(f);
    const empty = review(f, 'set_client', { client: 'New@Example.Test', allow_mode: 'only', allow: '', capped: 'inherit', mcp_profile: 'essentials' });
    assert.equal(await f.control.save(empty), true);
    const created = f.config().clients.find(row => row.client === 'new@example.test');
    assert.deepEqual(created.allow, []); assert.equal(created.capped, null); assert.equal(created.mcp_profile, 'essentials');
    await reload(f);
    assert.equal(await f.control.save(review(f, 'set_client', { allow_mode: 'inherit', deny: 'volume\ndj', grant: 'auth', manage_others: true }, created.client)), true);
    assert.equal(f.config().clients.find(row => row.client === created.client).allow, null);
    await reload(f); assert.equal(await f.control.save(review(f, 'remove_client', {}, created.client)), true);
    assert.equal(f.config().clients.some(row => row.client === created.client), false);
    assert.equal(f.config().clients.find(row => row.client === '__proto__').allow.length, 0);
  });

  test(`${surface}: contextual client lowercase catches collisions and matches the daemon's accepted identity`, async () => {
    const snapshot = configuration();
    snapshot.clients.push({ client: 'ος', allow: ['read'], deny: ['volume'], grant: ['auth'],
      capped: true, manage_others: false, mcp_profile: 'full' });
    const f = await fixture(surface, { snapshot }).ready();
    f.control.begin('set_client'); f.control.edit('client', 'ΟΣ');
    assert.equal(f.control.review(), null); assert.equal(f.writes().length, 0);
    assert.deepEqual(f.config().clients.find(row => row.client === 'ος').deny, ['volume']);
    const planned = review(f, 'set_client', { client: 'ΜΟΣ', allow_mode: 'only', allow: 'read', capped: 'capped' });
    assert.equal(planned.expected.clients.find(row => row.client === 'μος').allow[0], 'read');
    assert.equal(await f.control.save(planned), true);
    assert.equal(f.control.state.result.receipt.configuration.clients.some(row => row.client === 'μος'), true);
    const ambiguous = configuration();
    ambiguous.clients.push({ client: 'ΟΣ', deny: [] }, { client: 'ος', deny: [] });
    assert.throws(() => M.configurationOf(ambiguous));
  });

  test(`${surface}: a permission refusal exposes no configuration or edit controls`, async () => {
    const f = fixture(surface); f.read(() => { throw Object.assign(new Error('Owner or administrator required.'), { code: 'POLICY_DENIED', status: 403 }); });
    f.control.open(); await settled(() => !f.control.state.loading);
    assert.equal(f.control.state.loaded, false); assert.equal(f.control.state.configuration, null);
    assert.equal(f.control.begin('set_client'), false); assert.equal(f.control.review(), null); assert.equal(f.writes().length, 0);
    assert.match(f.control.state.message, /administrator/);
  });

  test(`${surface}: invalid values and an existing identity disguised by case cannot reach a review`, async () => {
    const f = await fixture(surface).ready();
    for (const value of ['-1', '101', '1.5', '1e2', '', 'NaN']) {
      f.control.begin('set_defaults'); f.control.edit('max_volume', value); assert.equal(f.control.review(), null);
    }
    f.control.begin('set_quiet_hours'); f.control.edit('start', '24:00'); assert.equal(f.control.review(), null);
    f.control.begin('set_client'); f.control.edit('client', 'AGENT@example.test'); assert.equal(f.control.review(), null);
    f.control.begin('set_room'); f.control.edit('room', "OWNER'S STUDY"); assert.equal(f.control.review(), null);
    assert.equal(f.writes().length, 0);
  });

  test(`${surface}: new server revisions or changed values invalidate a review before its write`, async () => {
    for (const mutate of [config => { config.revision = 'edited-elsewhere'; }, config => { config.defaults.max_volume = 14; }]) {
      const f = await fixture(surface).ready(), planned = review(f, 'set_defaults', { max_volume: '45' });
      const updated = f.config(); mutate(updated); f.setConfig(updated);
      assert.equal(await f.control.save(planned), false); assert.equal(f.writes().length, 0);
      assert.equal(f.control.state.result.status, 'not_sent'); assert.equal(f.control.state.draft.max_volume, '45');
    }
  });

  test(`${surface}: lost administration between review and send stops before POST`, async () => {
    const f = await fixture(surface).ready(), planned = review(f, 'set_defaults', { max_volume: '45' });
    f.read(() => { throw Object.assign(new Error('Administration was revoked.'), { status: 403, code: 'POLICY_DENIED' }); });
    assert.equal(await f.control.save(planned), false); assert.equal(f.writes().length, 0); assert.equal(f.control.state.result.status, 'not_sent');
  });

  test(`${surface}: conflict and lost acknowledgement never replay, and only explicit policy refresh resolves uncertainty`, async () => {
    for (const conflict of [true, false]) {
      const f = await fixture(surface).ready(), planned = review(f, 'set_defaults', { max_volume: '45' });
      f.write((url, body, write) => {
        if (!conflict) write(url, body);
        throw Object.assign(new Error(conflict ? 'Changed on the house.' : 'Connection ended after publication.'), conflict ? { status: 409, code: 'CONFIG_CHANGED' } : {});
      });
      assert.equal(await f.control.save(planned), false); assert.equal(f.writes().length, 1);
      assert.equal(await f.control.save(planned), false); assert.equal(f.control.begin('set_room'), false);
      assert.equal(f.control.state.result.status, conflict ? 'conflict' : 'uncertain');
      f.control.close(); f.control.open(); await settled(() => !f.control.state.loading);
      assert.ok(f.control.state.recovery); if (f.controller) assert.equal(f.controller.state.uncertain, true);
      await reload(f); assert.equal(f.control.state.recovery, null); assert.equal(f.control.state.result.status, 'observed');
      if (f.controller) assert.equal(f.controller.state.uncertain, false);
      assert.equal(f.writes().length, 1); assert.match(f.control.state.result.detail, /does not prove/);
    }
  });

  test(`${surface}: a success with unrelated dropped client rules remains uncertain`, async () => {
    const f = await fixture(surface).ready(), planned = review(f, 'set_defaults', { max_volume: '45' });
    f.write((url, body, write) => { const saved = write(url, body); saved.configuration.clients = []; return saved; });
    assert.equal(await f.control.save(planned), false); assert.equal(f.control.state.result.status, 'uncertain');
    assert.equal(f.writes().length, 1); assert.ok(f.control.state.draft);
  });

  test(`${surface}: hiding before dispatch retires the review while a sent write keeps its receipt`, async () => {
    const f = await fixture(surface).ready(), planned = review(f, 'set_defaults', { max_volume: '45' }), waiting = deferred();
    f.read(async (_url, _options, read) => { await waiting.promise; return read('/policy/configuration'); });
    const saving = f.control.save(planned); await settled(() => f.control.state.writing); f.control.suspend(); waiting.resolve();
    assert.equal(await saving, false); assert.equal(f.writes().length, 0);
    const g = await fixture(surface).ready(), second = review(g, 'set_defaults', { max_volume: '33' }), ack = deferred();
    g.write(async (url, body, write) => { const result = write(url, body); await ack.promise; return result; });
    const sent = g.control.save(second); await settled(() => g.writes().length === 1); g.control.suspend(); ack.resolve();
    assert.equal(await sent, true); assert.equal(g.control.state.open, false); assert.equal(g.control.state.result.status, 'accepted');
    assert.equal(g.writes().length, 1); assert.equal(await g.control.save(second), false);
  });

  test(`${surface}: visible form exposes caps and complete client rules as text with explicit confirmation`, async () => {
    const snapshot = configuration(); snapshot.clients[0].client = '<svg onload=alert(1)>@example.test';
    const f = await fixture(surface, { snapshot, dom: true }).ready();
    assert.match(f.panel.node.textContent, /House limits & permissions/); assert.match(f.panel.node.textContent, /<svg onload=alert\(1\)>/);
    assert.equal(f.control.begin('set_client', snapshot.clients[0].client), true);
    const fields = f.panel.node.querySelectorAll('[data-policy-config-focus]');
    const get = key => fields.find(node => node.getAttribute('data-policy-config-focus') === key);
    for (const key of ['allow_mode', 'allow', 'deny', 'grant', 'capped', 'manage_others', 'mcp_profile']) assert.ok(get(key), key);
    assert.equal(get('client').readOnly, true); get('capped').value = 'uncapped'; get('capped').fire('change');
    const reviewButton = f.panel.node.querySelectorAll('[data-policy-config-focus]').find(node => node.getAttribute('data-policy-config-focus') === 'review');
    reviewButton.fire('click'); assert.equal(f.writes().length, 0); assert.match(f.panel.node.textContent, /Save this rule for the house/);
    const confirm = f.panel.node.querySelectorAll('[data-policy-config-focus]').find(node => node.getAttribute('data-policy-config-focus') === 'confirm');
    confirm.fire('click'); await settled(() => !f.control.state.writing); assert.equal(f.writes().length, 1);
    assert.equal(f.control.state.result.status, 'accepted');
  });

  test(`${surface}: design previews remain read-only`, async () => {
    const f = await fixture(surface, { demo: true }).ready();
    assert.equal(f.control.editable(), false); assert.equal(f.control.begin('set_defaults'), false); assert.equal(f.writes().length, 0);
  });
}

test('wire decoder rejects unknown settings, malformed limits, and duplicate identities without silently dropping them', () => {
  for (const mutate of [value => { value.owner = 'forged'; }, value => { value.defaults.max_volume = 101; },
    value => { value.rooms.push({ room: 'OFFLINE CABIN', max_volume: 20 }); }, value => { value.clients[0].mcp_profile = 'secret'; },
    value => { value.clients[0].new_permission = true; }, value => { value.clients[0].allow = 'read'; }]) {
    const value = configuration(); mutate(value); assert.throws(() => M.configurationOf(value));
  }
});

test('dashboard uncertainty from policy writes cannot be cleared by inventory or another stored collection', async () => {
  const f = await fixture('dashboard').ready();
  f.write(() => { throw new Error('lost policy acknowledgement'); });
  assert.equal(await f.control.save(review(f, 'set_defaults', { max_volume: '44' })), false);
  assert.equal(f.controller.state.uncertain, true);
  await f.controller.refresh(true); assert.equal(f.controller.state.uncertain, true);
  await f.controller.refreshStored('schedules', value => value, { explicit: true }); assert.equal(f.controller.state.uncertain, true);
  await reload(f); assert.equal(f.controller.state.uncertain, false);
});

test('browser request adapter carries one reviewed policy edit over native GET/POST endpoints', async () => {
  const serverFixture = fixture('phone'), received = [];
  const server = http.createServer(async (request, response) => {
    try {
      let body = ''; for await (const chunk of request) body += chunk;
      received.push({ method: request.method, url: request.url, body: body ? JSON.parse(body) : undefined });
      const result = await serverFixture.raw(request.url, body ? JSON.parse(body) : undefined);
      response.setHeader('Content-Type', 'application/json'); response.end(JSON.stringify(result));
    } catch (error) { response.statusCode = 500; response.end(JSON.stringify({ detail: error.message })); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const base = `http://127.0.0.1:${server.address().port}`;
    const call = C.request({ fetcher: (url, options) => fetch(base + url, options) });
    const control = M.model({ call }); control.open(); await settled(() => control.state.loaded);
    control.begin('set_quiet_hours'); control.edit('max_volume', '17'); const planned = control.review();
    assert.equal(await control.save(planned), true);
    assert.deepEqual(received.map(row => [row.method, row.url]), [['GET', M.path], ['GET', M.path], ['POST', M.path]]);
    assert.equal(received[2].body.expected_revision, 'policy-review-A'); assert.equal(received[2].body.change.max_volume, 17);
    assert.equal(serverFixture.config().quiet_hours.max_volume, 17);
  } finally { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
});

test('both shipped remotes include the editor, strict stored route, lifecycle hooks, and discoverable house control', () => {
  const read = path => fs.readFileSync(new URL('../' + path, 'file://' + __filename), 'utf8');
  const remote = read('crates/fsonos-api/src/http/remote.rs'), dashboard = read('crates/fsonos-api/src/http/remote/dashboard.rs');
  for (const asset of ['policy-configuration.js', 'policy-configuration-panel.js', 'policy-configuration.css']) {
    assert.ok(remote.includes(asset), asset); assert.ok(dashboard.includes(asset), asset);
  }
  assert.match(remote, /\{POLICY_CONFIGURATION_SCRIPT\}\\n\{POLICY_CONFIGURATION_PANEL_SCRIPT\}/);
  assert.match(dashboard, /\{POLICY_CONFIGURATION\}\\n\{POLICY_CONFIGURATION_PANEL\}/);
  const entry = read('crates/fsonos-api/src/remote/dashboard.js');
  for (const method of ['suspend', 'resume', 'observe']) assert.ok(entry.includes(`policyConfiguration?.${method}()`));
  assert.match(entry, /refreshStored\('policyConfiguration'/); assert.match(entry, /stored: 'policyConfiguration'/);
  assert.equal(Studio.tools.find(tool => tool.id === 'policy-configuration').workspace, 'house');
  assert.ok(Studio.search('quiet hours', Studio.tools).some(tool => tool.id === 'policy-configuration'));
  assert.match(read('.github/workflows/linux-speech.yml'), /node --test [^\n]*scripts\/test-policy-configuration.cjs/);
});
