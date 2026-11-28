// Drive the shipped queue-copy controls and both remotes' selection handoffs.
// All queues, room identities and replies are synthetic; no speakers are used.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const Panel = require('../crates/fsonos-api/src/remote/queue-copy-panel.js');
const Copy = require('../crates/fsonos-api/src/remote/queue-copy.js');
const phoneQueueSource = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/queue.js'), 'utf8');
const DashboardQueue = require('../crates/fsonos-api/src/remote/dashboard-queue.js');
const Core = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const clone = value => structuredClone(value);
const tick = () => new Promise(setImmediate);
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
async function until(check) {
  for (let n = 0; n < 600 && !check(); n++) await Promise.resolve();
  assert.ok(check(), 'reached the expected UI or command boundary');
}

function documentFixture(t) {
  const walk = node => [node, ...node.children.flatMap(walk)];
  const matches = (node, selectors) => selectors.split(',').some(selector => {
    selector = selector.trim();
    if (selector === '[hidden]') return node.hidden;
    if (selector === '[inert]') return node.inert;
    if (selector[0] === '.') return (node.className || '').split(/\s+/).includes(selector.slice(1));
    const attr = /^\[([^=\]]+)(?:="([^"]*)")?\]$/.exec(selector);
    return attr ? node.getAttribute(attr[1]) !== null && (attr[2] === undefined || node.getAttribute(attr[1]) === attr[2])
      : node.tagName === selector.toUpperCase();
  });
  const events = target => {
    const listeners = new Map();
    return Object.assign(target, {
      addEventListener(name, listener) { if (!listeners.has(name)) listeners.set(name, new Set()); listeners.get(name).add(listener); },
      removeEventListener(name, listener) { listeners.get(name)?.delete(listener); },
      dispatchEvent(event) { for (const listener of listeners.get(event.type) || []) listener(event); return !event.defaultPrevented; },
      fire(type, extra = {}) { return this.dispatchEvent({ type, target: this, isComposing: false, preventDefault() { this.defaultPrevented = true; }, ...extra }); },
    });
  };
  const doc = events({ hidden: false, activeElement: null, defaultView: events({}) });
  doc.createElement = tag => events({ tagName: tag.toUpperCase(), ownerDocument: doc, children: [], parentElement: null,
    attributes: {}, dataset: {}, className: '', value: '', disabled: false, hidden: false, inert: false, open: false, checked: false,
    get textContent() { return (this._text || '') + this.children.map(child => child.textContent).join(''); },
    set textContent(value) { this._text = String(value); this.children = []; },
    setAttribute(key, value) { this.attributes[key] = String(value); },
    getAttribute(key) { return this.attributes[key] ?? null; }, removeAttribute(key) { delete this.attributes[key]; },
    append(...children) {
      for (const value of children) {
        if (value === null || value === undefined) continue;
        const child = typeof value === 'object' ? value : Object.assign(doc.createElement('#text'), { textContent: String(value) });
        child.parentElement = this; this.children.push(child);
      }
    },
    replaceChildren(...children) { for (const child of this.children) child.parentElement = null; this.children = []; this._text = ''; this.append(...children); },
    contains(node) { return walk(this).includes(node); },
    closest(selector) { for (let node = this; node; node = node.parentElement) if (matches(node, selector)) return node; return null; },
    querySelectorAll(selector) { return walk(this).slice(1).filter(node => matches(node, selector)); },
    querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
    focus() { doc.activeElement = this; }, scrollIntoView() {},
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; },
    click() { if (!this.disabled) this.fire('click'); },
  });
  const phoneEl = (tag, attrs = {}, ...children) => {
    const node = doc.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (value == null) continue;
      if (key === 'text') node.textContent = value;
      else if (key === 'class') node.className = value;
      else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
      else if (key.startsWith('data-') || key.startsWith('aria-')) node.setAttribute(key, value);
      else node[key] = value;
    }
    node.append(...children); return node;
  };
  const dashboardEl = (tag, cls = '', text) => { const node = doc.createElement(tag); node.className = cls; if (text != null) node.textContent = text; return node; };
  const before = global.document; global.document = doc; t.after(() => { global.document = before; });
  const host = doc.createElement('main');
  const find = (node, label) => walk(node).find(child => child.tagName === 'BUTTON' && child.textContent === label);
  return { doc, host, phoneEl, dashboardEl, walk, find };
}

function fixture(home = 'S2') {
  const zones = [
    { household: home, coordinator_room: 'Den', members: ['Den', 'Side'], transport_state: 'paused' },
    { household: home, coordinator_room: 'Office', members: ['Office'], transport_state: 'paused' },
    { household: home, coordinator_room: 'Kitchen', members: ['Kitchen'], transport_state: 'paused' },
  ];
  const tracks = [
    { position: 1, title: 'First local occurrence', uri: 'x-file-cifs://fixture.invalid/Music/repeated.flac', creator: 'Example artist', album: 'First album' },
    { position: 2, title: 'Second occurrence, same resource', uri: 'x-file-cifs://fixture.invalid/Music/repeated.flac', creator: 'Example artist', album: 'Second album' },
    { position: 3, title: 'Native service <track>', uri: 'x-sonos-http:opaque%3Atrack.mp3?sid=fixture&flags=8224&sn=1', creator: 'Native artist', album: 'Service collection' },
  ];
  const f = { source: `Den@${home}`, zones, tracks, calls: [], writes: [], lanes: [], completed: [], reader: null, writer: null, onSend: null };
  const id = (room, household = home) => `fixture-${household}-${room}`;
  const group = target => zones.find(zone => zone.members.some(room => `${room}@${zone.household}` === target));
  const targets = target => target === 'downstairs' ? zones.slice(1) : [group(target)].filter(Boolean);
  const room = (zone, name) => ({ room: name, household: zone.household, player: id(name, zone.household), coordinator: id(zone.coordinator_room, zone.household) });
  const resolution = target => ({ target, matched: group(target) ? 'room' : 'alias', revision: 'a'.repeat(64),
    rooms: targets(target).map(zone => room(zone, zone.members.find(name => `${name}@${zone.household}` === target) || zone.coordinator_room)),
    groups: targets(target).map(zone => ({ household: zone.household, coordinator_room: zone.coordinator_room,
      coordinator: id(zone.coordinator_room, zone.household), members: zone.members.map(name => room(zone, name)) })),
  });
  f.queues = new Map(zones.map(zone => [`${zone.coordinator_room}@${home}`, {
    zone: zone.coordinator_room, revision: `q1:${home}:${zone.coordinator_room}:7:3`, total: 3, playing: 1, items: clone(tracks),
  }]));
  const read = (route, options) => {
    if (route === 'zones') return clone(zones);
    if (route === 'sourceTargetResolve') return resolution(options.query.target);
    if (route === 'state') {
      const zone = group(options.params.room);
      return { zone: clone(zone), transport_state: 'paused', volume: 20, mute: false,
        track: { uri: 'fixture:current', title: 'Current song', queue_position: 1, duration_secs: 180, position_secs: 10 } };
    }
    if (['queue', 'sourceQueueState'].includes(route)) {
      const zone = group(options.params.room), queue = f.queues.get(`${zone.coordinator_room}@${home}`);
      return { ...clone(queue), items: clone(queue.items.slice(Number(options.query.offset), Number(options.query.offset) + Number(options.query.limit))) };
    }
    throw new Error('Unexpected read ' + route);
  };
  const receipt = body => {
    const destinations = targets(body.target), single = destinations.length === 1 ? destinations[0] : null;
    const response = { done: 'Copied selected queue occurrences', changed: true, notes: [], source_zone: body.zone,
      zone: single ? single.coordinator_room : body.target, positions: clone(body.positions),
      titles: body.positions.map(position => tracks[position - 1].title), mode: body.mode };
    if (body.mode === 'play') return { ...response, ...(single ? {} : { batch: { complete: true,
      results: destinations.map(zone => ({ household: home, target: id(zone.coordinator_room), zone: zone.coordinator_room,
        result: { status: 'succeeded', done: 'Selection playing', changed: true, notes: [] } })) } }) };
    return { ...response, queue_batch: { done: 'Selection added', complete: true, logged: true, undoable: true, failed: [],
      added: destinations.map(zone => ({ household: home, coordinator: id(zone.coordinator_room), queue: {
        zone: zone.coordinator_room, done: 'Added selected occurrences', count: body.positions.length,
        first: body.mode === 'next' ? 2 : 4, playing_queue: true,
      } })) } };
  };
  f.namedCall = async (route, options = {}) => {
    f.calls.push({ route, options });
    if (!options.body) return f.reader ? f.reader(route, options, read) : read(route, options);
    assert.equal(route, 'queueCopy', 'all speaker writes use the queue-copy endpoint');
    f.onSend?.(options.body); f.writes.push(clone(options.body));
    return f.writer ? f.writer(options.body, receipt) : receipt(options.body);
  };
  f.call = async (url, body) => {
    const parsed = new URL(url, 'http://fixture'), query = Object.fromEntries(parsed.searchParams);
    if (body !== undefined) { assert.equal(parsed.pathname, '/queue/selection'); return f.namedCall('queueCopy', { body }); }
    if (parsed.pathname === '/zones') return f.namedCall('zones');
    if (parsed.pathname === '/rooms/resolve') return f.namedCall('sourceTargetResolve', { query });
    const match = /^\/zones\/([^/]+)\/(state|queue)$/.exec(parsed.pathname);
    assert.ok(match, 'known read ' + url);
    return f.namedCall(match[2] === 'queue' ? 'sourceQueueState' : 'state', { params: { room: decodeURIComponent(match[1]) }, query });
  };
  f.selection = () => ({ zone: f.source, revision: f.queues.get(f.source).revision, total: 3, tracks: clone([tracks[0], tracks[2]]) });
  f.executeSet = async (groups, work) => { f.lanes.push(groups); return work(); };
  return f;
}

function mounted(dom, f) {
  const view = Panel.panel({ call: f.namedCall, executeSet: f.executeSet, document: dom.doc });
  dom.host.append(view.node); return view;
}
function input(view, label, value, event = 'input') {
  const node = view.node.querySelector(`[aria-label="${label}"]`); assert.ok(node, label);
  assert.equal(node.disabled, false, label + ' is editable'); node.value = value; node.fire(event); return node;
}
async function review(dom, view, target = 'downstairs', mode = 'end') {
  input(view, 'Queue copy destination', target); input(view, 'Queue copy action', mode, 'change');
  dom.find(view.node, 'Review copy').click(); await until(() => !view.state.reading);
  assert.ok(view.state.pending, view.state.message); return view.state.pending;
}

test('phone queue checkboxes hand off exact native occurrences; reorder/repeat and room-set confirmation send once', async t => {
  const dom = documentFixture(t), f = fixture('S1'), view = mounted(dom, f);
  const PhoneQueue = vm.runInNewContext(phoneQueueSource + '\nQueueRemote;', { document: dom.doc, CustomEvent });
  const queue = PhoneQueue.panel({ zone: f.source, call: f.namedCall, el: dom.phoneEl, current: () => true });
  t.after(() => { queue.dispose(); view.dispose(); }); dom.host.append(queue.node);
  queue.node.open = true; queue.node.fire('toggle'); await tick();
  for (const position of [1, 3]) {
    const checkbox = queue.node.querySelector(`[data-focus="select-${position}"]`);
    assert.ok(checkbox); checkbox.checked = true; checkbox.fire('change');
  }
  const handoff = dom.find(queue.node, 'Copy selected to rooms…'); assert.equal(handoff.disabled, false); handoff.click();
  await until(() => view.state.open && !view.state.reading);
  assert.deepEqual(clone(view.state.selection), Copy.selectionOf(f.selection()));
  assert.equal(f.writes.length, 0); assert.equal(f.lanes.length, 0);
  const occurrence = (label, number) => dom.walk(view.node).find(node => node.getAttribute('aria-label')?.startsWith(`${label} copied occurrence ${number}:`));
  occurrence('Repeat', 2).click(); occurrence('Move up', 3).click(); occurrence('Move down', 1).click();
  assert.deepEqual(clone(view.state.selection.tracks.map(track => track.position)), [3, 1, 3]);
  assert.deepEqual(view.node.querySelector('[aria-label="Tracks to copy in order"]').children.map(row => row.querySelector('strong').textContent),
    ['#3 · Native service <track>', '#1 · First local occurrence', '#3 · Native service <track>']);
  assert.equal(view.node.querySelectorAll('track').length, 0, 'native titles are rendered as text');
  await review(dom, view);
  const consent = view.node.querySelector('[aria-label="Review queue copy"]');
  assert.match(consent.textContent, /Office@S1/); assert.match(consent.textContent, /Kitchen@S1/);
  assert.match(consent.textContent, /without starting or resuming playback/);
  assert.equal(f.writes.length, 0, 'selection, ordering and complete destination review are read-only');
  const confirm = dom.find(view.node, 'Copy into queues'); confirm.click(); confirm.click();
  await until(() => !view.state.writing);
  assert.equal(view.state.result.complete, true, view.state.result.detail);
  assert.equal(f.writes.length, 1); assert.equal(f.lanes.length, 1);
  assert.deepEqual(f.writes[0], { zone: f.source, positions: [3, 1, 3], expected_revision: f.selection().revision,
    mode: 'append', target: 'downstairs', expected_target_revision: 'a'.repeat(64) });
  assert.deepEqual(f.lanes[0].map(group => [group.target, group.sourceOnly === true]), [['Kitchen@S1', false], ['Office@S1', false], ['Den@S1', true]]);
  assert.deepEqual(f.queues.get(f.source).items, f.tracks, 'authoring does not edit the source queue');
});

async function dashboard(dom, f) {
  let queue, editor;
  const controller = Core.model({ call: f.call, now: () => 1000,
    changed: () => { if (queue) { queue.observe(controller.selected()); editor?.update(); } } });
  await controller.refresh(); controller.connection(true); controller.select(f.source);
  queue = DashboardQueue.model({ call: f.call, changed: () => editor?.update() });
  queue.observe(controller.selected()); queue.open(); await tick();
  editor = DashboardQueue.editor({ queue, controller, session: { state: { pending: null }, reviewQueue() { throw new Error('Copying must use its separate review'); } }, el: dom.dashboardEl });
  const view = Panel.dashboard({ call: f.call, controller, document: dom.doc, completed: targets => f.completed.push(targets) });
  dom.host.append(editor.node, view.node);
  f.onSend = () => assert.equal(controller.state.busy, true, 'the real dashboard command lane owns the write');
  return { controller, queue, editor, view };
}

test('dashboard range editor hands off to the copy panel and retains the command lane through acknowledgement', async t => {
  const dom = documentFixture(t), f = fixture(), d = await dashboard(dom, f);
  t.after(() => { d.view.dispose(); d.queue.dispose(); d.controller.dispose(); });
  for (const [label, value] of [['First position', '1'], ['Last position', '3']]) {
    const field = d.editor.node.querySelector(`[aria-label="${label}"]`); field.value = value; field.fire('input');
  }
  const handoff = dom.find(d.editor.node, 'Copy selected to rooms…'); assert.equal(handoff.disabled, false); handoff.click();
  await until(() => d.view.state.open && !d.view.state.reading);
  assert.deepEqual(d.view.state.selection.tracks.map(track => track.position), [1, 2, 3]);
  assert.equal(d.view.state.selection.tracks[0].uri, d.view.state.selection.tracks[1].uri, 'equal resource URIs retain both occurrences');
  await review(dom, d.view, 'downstairs', 'play');
  assert.equal(f.writes.length, 0); assert.equal(d.controller.state.busy, false);
  const hold = deferred(); f.writer = async (body, receipt) => { await hold.promise; return receipt(body); };
  const confirm = dom.find(d.view.node, 'Copy and play'); confirm.click(); await until(() => f.writes.length === 1);
  assert.equal(d.controller.state.busy, true); assert.equal(await d.controller.command('pause'), false);
  confirm.click(); assert.equal(f.writes.length, 1);
  hold.resolve(); await until(() => !d.view.state.writing);
  assert.equal(d.controller.state.busy, false); assert.equal(d.controller.state.uncertain, false);
  assert.equal(d.view.state.result.complete, true, d.view.state.result.detail);
  assert.deepEqual(f.completed, [['Kitchen@S2', 'Office@S2']]);
  assert.deepEqual(f.writes[0].positions, [1, 2, 3]); assert.equal(f.writes[0].mode, 'play');
});

test('cancel and close consume the rendered review without writing or reusing its old confirm button', async t => {
  const dom = documentFixture(t), f = fixture(), view = mounted(dom, f); t.after(() => view.dispose());
  assert.equal(await view.open(f.selection()), true); await review(dom, view);
  const old = dom.find(view.node, 'Copy into queues'); dom.find(view.node, 'Cancel copy review').click(); old.click();
  assert.equal(view.state.pending, null); assert.equal(f.writes.length, 0);
  await review(dom, view); const closed = dom.find(view.node, 'Copy into queues'); dom.find(view.node, 'Close copy').click(); closed.click();
  assert.equal(view.state.open, false); assert.equal(view.node.hidden, true); assert.equal(f.writes.length, 0);
});

test('removing the final copied occurrence keeps an empty draft and disables review', async t => {
  const dom = documentFixture(t), f = fixture(), view = mounted(dom, f); t.after(() => view.dispose());
  assert.equal(await view.open({ ...f.selection(), tracks: [f.tracks[0]] }), true);
  dom.find(view.node, 'Remove').click();
  assert.equal(view.state.selection.tracks.length, 0); assert.equal(dom.find(view.node, 'Review copy').disabled, true);
  assert.match(view.node.textContent, /Choose tracks from the source queue/); assert.equal(f.writes.length, 0);
});

for (const hidden of ['document', 'ancestor', 'suspended']) {
  test(`${hidden} UI during confirmation rereads stops the queued write`, async t => {
    const dom = documentFixture(t), f = fixture(), view = mounted(dom, f); t.after(() => view.dispose());
    assert.equal(await view.open(f.selection()), true); await review(dom, view);
    const hold = deferred(); let reading = false;
    f.reader = async (route, options, read) => { reading = true; await hold.promise; return read(route, options); };
    const confirm = dom.find(view.node, 'Copy into queues'); confirm.click(); await until(() => reading);
    if (hidden === 'document') { dom.doc.hidden = true; dom.doc.fire('visibilitychange'); }
    else if (hidden === 'ancestor') dom.host.hidden = true;
    else view.suspend();
    hold.resolve(); await until(() => !view.state.writing);
    assert.equal(f.writes.length, 0); assert.equal(view.state.pending, null); confirm.click(); assert.equal(f.writes.length, 0);
    assert.equal(view.state.result.status, 'not_sent');
  });
}

test('hidden and disposed hosts decline handoffs and do not start source reads', async t => {
  const dom = documentFixture(t), f = fixture(), view = mounted(dom, f);
  const send = () => dom.doc.dispatchEvent(new CustomEvent('fsonos:queue-selection', { cancelable: true, detail: { selection: f.selection() } }));
  dom.host.hidden = true; assert.equal(send(), true); assert.equal(f.calls.length, 0);
  dom.host.hidden = false; dom.doc.hidden = true; assert.equal(send(), true); assert.equal(f.calls.length, 0);
  dom.doc.hidden = false; view.dispose(); assert.equal(send(), true); assert.equal(f.calls.length, 0);
});

test('a dashboard acknowledgement survives hiding after dispatch and cannot be sent twice', async t => {
  const dom = documentFixture(t), f = fixture(), d = await dashboard(dom, f);
  t.after(() => { d.view.dispose(); d.queue.dispose(); d.controller.dispose(); });
  assert.equal(await d.view.open(f.selection()), true); await review(dom, d.view, 'Office@S2', 'next');
  const hold = deferred(); f.writer = async (body, receipt) => { await hold.promise; return receipt(body); };
  const confirm = dom.find(d.view.node, 'Copy into queues'); confirm.click(); await until(() => f.writes.length === 1);
  dom.doc.hidden = true; dom.doc.fire('visibilitychange'); d.controller.suspend(); hold.resolve();
  await until(() => !d.view.state.writing);
  assert.equal(d.view.state.result.complete, true, d.view.state.result.detail);
  assert.equal(d.controller.state.uncertain, false); assert.equal(f.writes.length, 1);
  dom.doc.hidden = false; dom.doc.fire('visibilitychange'); confirm.click(); assert.equal(f.writes.length, 1);
});

test('a dashboard lost acknowledgement is shown as uncertain and keeps the shared command lane blocked', async t => {
  const dom = documentFixture(t), f = fixture(), d = await dashboard(dom, f);
  t.after(() => { d.view.dispose(); d.queue.dispose(); d.controller.dispose(); });
  assert.equal(await d.view.open(f.selection()), true); await review(dom, d.view, 'Office@S2', 'play');
  f.writer = () => { throw new Error('Connection lost after queue-copy dispatch'); };
  const confirm = dom.find(d.view.node, 'Copy and play'); confirm.click(); await until(() => !d.view.state.writing);
  assert.equal(f.writes.length, 1); assert.equal(d.view.state.result.status, 'uncertain'); assert.equal(d.controller.state.uncertain, true);
  assert.equal(await d.controller.command('pause'), false); confirm.click(); assert.equal(f.writes.length, 1);
  assert.match(d.view.node.querySelector('.queue-copy-result').textContent, /not repeated/);
});
