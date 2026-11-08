'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Panel = require('../crates/fsonos-api/src/remote/dashboard-playlist-save-panel.js');
const tick = () => new Promise(resolve => setImmediate(resolve));
async function until(check) {
  for (let i = 0; i < 80 && !check(); i++) await tick();
  assert.ok(check(), 'the expected view state arrives');
}

function documentFixture() {
  const nodes = [];
  class Element {
    constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map(); this.events = new Map();
      this.dataset = {}; this.value = ''; this.open = false; this.isConnected = true; this._text = ''; nodes.push(this); }
    set textContent(value) { this._text = String(value); this.children = []; }
    get textContent() { return this._text + this.children.map(node => node.textContent).join(''); }
    append(...children) { for (const child of children) { child.parentElement = this; this.children.push(child); } }
    replaceChildren(...children) { this._text = ''; this.children = []; this.append(...children); }
    setAttribute(key, value) { this.attributes.set(key, String(value)); }
    getAttribute(key) { return this.attributes.get(key) ?? null; }
    addEventListener(key, fn) { const list = this.events.get(key) || []; list.push(fn); this.events.set(key, list); }
    removeEventListener(key, fn) { this.events.set(key, (this.events.get(key) || []).filter(item => item !== fn)); }
    fire(key) { for (const fn of this.events.get(key) || []) fn({ target: this, preventDefault() {} }); }
    click() { if (!this.disabled) this.fire('click'); }
    focus() { doc.activeElement = this; }
    showModal() { this.open = true; }
    close() { this.open = false; this.fire('close'); }
  }
  const doc = { hidden: false, activeElement: null, createElement: tag => new Element(tag),
    getElementById: id => nodes.find(node => node.id === id),
    querySelector: selector => selector === 'dialog[open]' ? nodes.find(node => node.tagName === 'DIALOG' && node.open) || null : null };
  const trigger = doc.createElement('button'); trigger.id = 'open-playlist-save';
  const dialog = doc.createElement('dialog'); dialog.id = 'playlist-save-dialog'; doc.activeElement = trigger;
  return { doc, nodes, trigger, dialog, $: id => doc.getElementById(id) };
}

async function setup({ logged = true, lost = false, browse } = {}) {
  const ui = documentFixture(), calls = [], saved = [], catalogs = new Map([['Studio@S2', []], ['Studio@S1', []]]);
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Studio', members: ['Studio'], transport_state: 'playing' }));
  let panel, seq = 0;
  const call = async (url, body) => {
    calls.push({ url, body });
    if (url === '/zones') return zones;
    if (url.endsWith('/state')) {
      const target = decodeURIComponent(url.split('/')[2]), zone = zones.find(item => C.key(item) === target);
      return { zone, transport_state: 'playing', volume: 20, mute: false, track: null };
    }
    if (url.includes('/queue?')) return { zone: 'Studio', total: 2, revision: 'retained-queue', playing: null,
      items: [{ position: 1, title: 'First entry', uri: 'fixture:first' }] };
    if (url === '/sonos/playlists/save') {
      const playlist = { id: 'SQ:' + (++seq), title: body.title };
      catalogs.get(body.zone).push(playlist);
      if (lost) throw new Error('The reply was lost after the native save.');
      return { done: 'Queue saved as a new Sonos playlist.', zone: 'Studio', playlist, logged };
    }
    if (url.startsWith('/sonos/playlists?')) return catalogs.get(new URL(url, 'http://fixture').searchParams.get('zone'));
    throw new Error('Unexpected request: ' + url);
  };
  const controller = C.model({ call, now: () => 0, changed: () => panel?.observe() });
  panel = Panel.panel({ call, controller, document: ui.doc, now: () => 0, browse,
    saved: value => saved.push(value) });
  await controller.refresh();
  const writes = () => calls.filter(item => item.body !== undefined);
  return { ...ui, panel, controller, calls, saved, catalogs, writes,
    async open(title = 'Sunday evening') {
      ui.trigger.click(); await until(() => panel.control.state.loaded);
      ui.$('playlist-save-name').value = title; ui.$('playlist-save-name').fire('input');
    },
    async review() { ui.$('playlist-save-form').fire('submit'); await until(() => panel.control.state.pending); },
    async save() { await this.review(); ui.$('playlist-save-confirm').click(); await until(() => panel.control.state.result && !panel.control.state.writing); },
    close() { panel.dispose(); controller.dispose(); },
  };
}

test('queue-save form reviews literal title and group before one native save without track metadata', async t => {
  const h = await setup(); t.after(h.close);
  const title = '<img src=x onerror=fixture()> & Sunday';
  await h.open('  ' + title + '  ');
  assert.equal(h.dialog.open, true); assert.equal(h.writes().length, 0);
  assert.match(h.$('playlist-save-observation').textContent, /2 entries/);
  assert.match(h.$('playlist-save-name-help').textContent, /removed/);
  await h.review();
  assert.equal(h.$('playlist-save-preview').hidden, false);
  assert.equal(h.$('playlist-save-preview-name').textContent, title);
  assert.match(h.$('playlist-save-preview-target').textContent, /Studio.*S2/);
  assert.equal(h.$('playlist-save-preview-queue').textContent, '2 entries');
  assert.equal(h.doc.activeElement, h.$('playlist-save-preview'));
  assert.equal(h.writes().length, 0);
  h.$('playlist-save-confirm').click(); await until(() => h.panel.control.state.result && !h.panel.control.state.writing);
  assert.deepEqual(h.writes(), [{ url: '/sonos/playlists/save', body: { zone: 'Studio@S2', title } }]);
  assert.equal(h.$('playlist-save-result-title').textContent, 'Playlist saved');
  assert.equal(h.$('playlist-save-result-name').textContent, title);
  assert.equal(h.$('playlist-save-result-id').textContent, 'SQ:1');
  assert.equal(h.$('playlist-save-log-warning').hidden, true);
  assert.equal(h.saved.length, 1);
  assert.equal(h.nodes.some(node => node.tagName === 'IMG'), false, 'native metadata stays text');
});

test('saved receipt warns about action-log failure and only explicitly opens the existing catalog browser', async t => {
  const browsed = [], h = await setup({ logged: false, browse: receipt => { browsed.push(receipt); return true; } }); t.after(h.close);
  await h.open(); await h.save();
  assert.equal(h.$('playlist-save-log-warning').hidden, false);
  assert.match(h.$('playlist-save-log-warning').textContent, /playlist is saved/);
  assert.equal(browsed.length, 0);
  h.$('playlist-save-browse').click(); await until(() => browsed.length === 1);
  assert.equal(h.dialog.open, false); assert.equal(browsed[0].playlist.id, 'SQ:1');
  assert.equal(h.writes().length, 1);
});

test('lost-reply recovery shows the original household catalog after selection changes without replaying', async t => {
  const h = await setup({ lost: true }); t.after(h.close);
  await h.open('Same name'); await h.save();
  assert.equal(h.controller.state.uncertain, true);
  assert.equal(h.$('playlist-save-check-catalog').hidden, false);
  assert.equal(h.$('playlist-save-read').disabled, true);
  assert.equal(h.$('playlist-save-review').disabled, true);
  assert.equal(h.controller.select('Studio@S1'), true);
  assert.match(h.$('playlist-save-target').textContent, /S1/);
  assert.match(h.$('playlist-save-result-target').textContent, /S2/);
  h.$('playlist-save-check-catalog').click(); await until(() => h.panel.control.state.catalogStatus === 'ready');
  assert.equal(h.calls.at(-1).url, '/sonos/playlists?zone=Studio%40S2');
  assert.equal(h.controller.state.uncertain, false);
  assert.equal(h.panel.control.state.result.status, 'uncertain');
  assert.equal(h.$('playlist-save-catalog').hidden, false);
  assert.match(h.$('playlist-save-catalog-scope').textContent, /Studio@S2/);
  assert.match(h.$('playlist-save-catalog-items').textContent, /Same nameSQ:1/);
  assert.match(h.$('playlist-save-recovery').textContent, /matching name cannot confirm/);
  assert.equal(h.$('playlist-save-review').disabled, true, 'a new queue observation is still required');
  assert.equal(h.writes().length, 1);
});

test('oversized titles remain a visible invalid draft and closure retires an unsent review', async t => {
  const h = await setup(); t.after(h.close);
  await h.open('é'.repeat(512));
  assert.equal(h.$('playlist-save-review').disabled, true);
  assert.match(h.$('playlist-save-name-help').textContent, /1023 UTF-8 bytes/);
  h.$('playlist-save-name').value = 'Valid name'; h.$('playlist-save-name').fire('input'); await h.review();
  h.$('close-playlist-save').click();
  assert.equal(h.dialog.open, false); assert.equal(h.panel.control.state.pending, null);
  h.$('playlist-save-confirm').click(); await tick(); assert.equal(h.writes().length, 0);
  h.panel.suspend(); assert.equal(h.panel.open(), false); assert.equal(h.dialog.open, false);
});

test('catalog browsing refusal preserves the saved receipt and explains how to keep an existing draft', async t => {
  const h = await setup({ browse: () => 'Finish the existing source draft first.' }); t.after(h.close);
  await h.open(); await h.save(); h.$('playlist-save-browse').click();
  await until(() => h.dialog.open && /existing source draft/.test(h.$('playlist-save-message').textContent));
  assert.equal(h.$('playlist-save-result-id').textContent, 'SQ:1');
  assert.equal(h.writes().length, 1);
});
