'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { fixture, target, defer, until } = require('./dj-targets-fixture.cjs');
const T = require('../crates/fsonos-api/src/remote/dj-targets.js');
const Panel = require('../crates/fsonos-api/src/remote/dj-targets-panel.js');
const Studio = require('../crates/fsonos-api/src/remote/studio.js');

function documentFixture() {
  const nodes = [];
  class Element {
    constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map(); this.events = new Map();
      this.value = ''; this.open = false; this.disabled = false; this.hidden = false; this._text = ''; nodes.push(this); }
    set textContent(value) { this._text = String(value); this.children = []; }
    get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(''); }
    append(...children) { for (const child of children) { if (typeof child !== 'string') child.parentElement = this; this.children.push(child); } }
    replaceChildren(...children) { this._text = ''; this.children = []; this.append(...children); }
    setAttribute(key, value) { this.attributes.set(key, String(value)); }
    getAttribute(key) { return this.attributes.get(key) ?? null; }
    addEventListener(name, fn) { const list = this.events.get(name) || []; list.push(fn); this.events.set(name, list); }
    fire(name) { for (const fn of this.events.get(name) || []) fn({ target: this, preventDefault() {} }); }
    click() { if (!this.disabled) this.fire('click'); }
    focus() { doc.activeElement = this; }
    showModal() { this.open = true; }
    close() { this.open = false; this.fire('close'); }
  }
  const doc = { hidden: false, activeElement: null, createElement: tag => new Element(tag),
    getElementById: id => nodes.find(node => node.id === id),
    querySelector: selector => selector === 'dialog[open]' ? nodes.find(node => node.tagName === 'DIALOG' && node.open) : null };
  const element = (id, tag) => { const node = doc.createElement(tag); node.id = id; return node; };
  element('open-dj-targets', 'button'); element('close-dj-targets', 'button'); element('dj-targets-dialog', 'dialog'); element('dj-targets-content', 'div');
  const input = suffix => nodes.find(node => node.id?.endsWith('-' + suffix) && /^dj-targets-\d+-/.test(node.id));
  const button = text => [...nodes].reverse().find(node => node.tagName === 'BUTTON' && node.textContent === text);
  const css = name => [...nodes].reverse().find(node => node.className === name);
  return { doc, nodes, input, button, css };
}
async function setup(dashboard = false) {
  const ui = documentFixture(), h = fixture({ dashboard });
  const view = dashboard ? Panel.dashboard({ call: h.http, controller: h.controller, document: ui.doc, now: () => h.time })
    : Panel.panel({ document: ui.doc, create: changed => T.model({ ...h.options, changed }) });
  h.control = view.control;
  if (dashboard) { await h.controller.refresh(); h.controller.connection(true); view.observe(); }
  const open = async () => {
    if (dashboard) ui.doc.getElementById('open-dj-targets').click(); else view.open();
    await until(() => !view.state.reading);
    ui.input('target').value = target; ui.input('target').fire('input');
  };
  const review = async () => { ui.css('dj-targets-form').fire('submit'); await until(() => view.state.pending || view.state.message && !view.state.reading); return view.state.pending; };
  return Object.assign(h, ui, { view, open, review });
}

test('the shared form exposes full affected groups, current sources and exact sessions as safe text', async () => {
  const h = await setup(); await h.open();
  assert.ok(h.input('suggestions').children.some(option => option.value === 'all@S1'));
  assert.equal(h.writes().length, 0); const pending = await h.review(); assert.ok(pending, h.view.state.message);
  const preview = h.css('dj-targets-review');
  assert.match(preview.textContent, /Den@S1/); assert.match(preview.textContent, /Den@S2/);
  assert.match(preview.textContent, /Also affected through grouping: Patio@S1/);
  assert.match(preview.textContent, /Current <music>/); assert.match(preview.textContent, /Current whole work: Current work/);
  assert.match(preview.textContent, /Next queued works: Next work/); assert.match(preview.textContent, /Temporary session/);
  assert.match(preview.textContent, /DJ idle/); assert.match(preview.textContent, /4 retained queue entries/);
  assert.equal(h.doc.activeElement, preview); assert.equal(h.nodes.some(node => node.tagName === 'MUSIC'), false);
  assert.equal(h.writes().length, 0);
});

test('typing shared direction fields retains the same focused nodes and sends exact comma-containing names', async () => {
  const h = await setup(); await h.open();
  h.input('direction-mode').value = 'replace'; h.input('direction-mode').fire('change');
  h.input('mood').value = 'focus'; h.input('mood').fire('change');
  const artists = h.input('filters-include_artists'); artists.focus(); artists.value = 'Earth, Wind & Fire\nBjörk'; artists.selectionStart = 9;
  artists.fire('input'); assert.equal(h.doc.activeElement, artists); assert.equal(h.input('filters-include_artists'), artists);
  assert.equal(artists.selectionStart, 9);
  const minutes = h.input('filters-minutes'); minutes.value = '90'; minutes.fire('input');
  const strict = h.input('filters-strict'); strict.checked = true; strict.fire('change');
  await h.review(); assert.match(h.css('dj-targets-review').textContent, /90 minutes from this command/);
  const send = h.button('Confirm: Start DJs'), reply = defer(); h.reply = () => reply.promise;
  send.click(); await until(() => h.writes().length === 1);
  assert.equal(artists.disabled, true); assert.equal(h.input('target').disabled, true); send.click();
  assert.equal(h.writes().length, 1);
  assert.deepEqual(h.writes()[0].args.body.constraints, { include_artists: ['Earth, Wind & Fire', 'Björk'], strict: true });
  assert.equal(h.writes()[0].args.body.for_secs, 5400);
  reply.resolve({ done: 'Den@S1: started; Den@S2: started; Kitchen@S2: started', changed: true, notes: [] });
  await until(() => !h.view.state.writing);
  assert.match(h.css('dj-targets-result').textContent, /DJ command accepted/);
  assert.equal(h.writes().length, 1);
});

test('cancelled and edited reviews cannot be submitted through detached confirm buttons', async () => {
  const h = await setup(); await h.open(); await h.review();
  const old = h.button('Confirm: Start DJs'); h.button('Cancel review').click(); old.click();
  assert.equal(h.writes().length, 0); assert.equal(h.view.state.pending, null);
  await h.review(); const changed = h.button('Confirm: Start DJs');
  h.input('action').value = 'clear'; h.input('action').fire('change'); changed.click();
  assert.equal(h.view.state.pending, null); assert.equal(h.writes().length, 0);
  await h.review(); h.button('Confirm: Return to programs').click(); await until(() => !h.view.state.writing && h.view.state.result);
  assert.equal(h.writes().length, 1); assert.equal(h.writes()[0].args.body.clear, true);
});

test('partial group receipts remain visible until an explicit read of the originally affected rooms', async () => {
  const h = await setup(); await h.open(); h.input('action').value = 'skip'; h.input('action').fire('change');
  await h.review(); h.reply = () => ({ done: 'Den@S1: skipped; Den@S2: skipped', changed: true,
    notes: [{ code: 'SKIPPED', detail: 'Kitchen@S2: DJ_NOT_RUNNING: no active feed' }] });
  h.button('Confirm: Skip current whole works').click(); await until(() => h.view.state.recovery);
  const receipt = h.css('dj-targets-result'); assert.match(receipt.textContent, /Incomplete DJ request/);
  assert.match(receipt.textContent, /SKIPPED: Kitchen@S2: DJ_NOT_RUNNING/); assert.match(receipt.textContent, /Do not repeat/);
  assert.equal(h.button('Review affected groups').disabled, true); assert.equal(h.button('Read affected rooms').hidden, false);
  h.button('Read affected rooms').click(); await until(() => !h.view.state.reading && !h.view.state.recovery);
  assert.match(h.css('dj-targets-observation').textContent, /Den@S1.*Den@S2.*Kitchen@S2/);
  assert.match(h.css('dj-targets-result').textContent, /SKIPPED: Kitchen@S2/);
  assert.equal(h.button('Review affected groups').disabled, false); assert.equal(h.writes().length, 1);
});

test('a lost response has a persistent recovery control and cannot turn reopening into a retry', async () => {
  const h = await setup(); await h.open(); await h.review(); h.reply = () => { throw new Error('The connection ended after dispatch'); };
  h.button('Confirm: Start DJs').click(); await until(() => h.view.state.result && !h.view.state.writing);
  assert.match(h.css('dj-targets-result').textContent, /DJ outcome uncertain/); assert.equal(h.css('dj-targets-result').getAttribute('role'), 'alert');
  h.view.close(); h.view.open(); await until(() => !h.view.state.reading);
  assert.equal(h.button('Review affected groups').disabled, true); assert.equal(h.writes().length, 1);
  h.button('Read affected rooms').click(); await until(() => !h.view.state.recovery && !h.view.state.reading);
  assert.match(h.css('dj-targets-result').textContent, /original receipt is retained/);
  assert.equal(h.writes().length, 1);
});

test('dashboard trigger and dialog use the same controller and command lane with focus recovery', async () => {
  const h = await setup(true), trigger = h.doc.getElementById('open-dj-targets'), dialog = h.doc.getElementById('dj-targets-dialog');
  trigger.focus(); await h.open(); assert.equal(dialog.open, true); assert.equal(trigger.getAttribute('aria-expanded'), 'true');
  await h.review(); h.button('Confirm: Start DJs').click(); await until(() => h.view.state.result && !h.view.state.writing);
  assert.equal(h.writes().length, 1); assert.equal(h.controller.state.busy, false);
  h.doc.getElementById('close-dj-targets').click(); assert.equal(dialog.open, false);
  assert.equal(trigger.getAttribute('aria-expanded'), 'false'); assert.equal(h.doc.activeElement, trigger);
});

test('unsupported daemon revisions produce an actionable form error without any confirmation or write', async () => {
  const h = await setup(); await h.open();
  h.custom = (name, args) => { if (name === 'djTargetsResolve') { const view = h.resolution(args.query.target); delete view.revision; return view; } };
  assert.equal(await h.review(), null); assert.match(h.css('dj-targets-message').textContent, /Update the daemon/);
  assert.equal(h.css('dj-targets-review').hidden, true); assert.equal(h.writes().length, 0);
});

test('both served bundles include shared review assets after their dependencies and the studio can find the tool', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src');
  const phone = fs.readFileSync(path.join(root, 'http/remote.rs'), 'utf8');
  const dashboard = fs.readFileSync(path.join(root, 'http/remote/dashboard.rs'), 'utf8');
  for (const [source, model, panel, dependency] of [[phone, 'DJ_TARGETS_SCRIPT', 'DJ_TARGETS_PANEL_SCRIPT', 'SOURCE_PLAYER_SCRIPT'],
    [dashboard, 'DJ_TARGETS', 'DJ_TARGETS_PANEL', 'SOURCE_PLAYER']]) {
    for (const file of ['dj-targets.js', 'dj-targets-panel.js', 'dj-targets.css']) assert.ok(source.includes('/' + file + '");'));
    const scripts = source.match(/"\{(?:EVENT_FEED_SCRIPT|EVENTS)\}[^"\n]+"/)[0];
    assert.ok(scripts.indexOf('{' + dependency + '}') < scripts.indexOf('{' + model + '}'));
    assert.ok(scripts.indexOf('{' + model + '}') < scripts.indexOf('{' + panel + '}'));
  }
  const html = fs.readFileSync(path.join(root, 'remote/dashboard.html'), 'utf8');
  for (const id of ['open-dj-targets', 'close-dj-targets', 'dj-targets-dialog', 'dj-targets-content']) assert.ok(html.includes(`id="${id}"`));
  const tool = Studio.identify({ id: 'dj-targets-controls', getAttribute: () => 'DJ across rooms', querySelector: () => null });
  assert.equal(tool.id, 'dj-targets'); assert.equal(tool.workspace, 'music');
  assert.equal(Studio.search('DJ everywhere', [tool]).length, 1);
  const code = fs.readFileSync(path.join(root, 'remote/dj-targets-panel.js'), 'utf8');
  assert.equal(code.includes('innerHTML'), false); assert.equal(code.includes('fetch('), false);
});
