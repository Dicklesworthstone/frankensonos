'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { moveFocus } = require('../crates/fsonos-api/src/remote/dashboard-workspace.js');
function fixture() {
  const focused = [], nodes = Array.from({ length: 3 }, (_, i) => ({ disabled: false, getClientRects: () => [{}], focus() { focused.push(i); } }));
  const input = {};
  const move = (key, target = input, extra = {}) => {
    const event = { key, target, preventDefault() { this.prevented = true; }, ...extra };
    moveFocus(event, nodes, input); return event;
  };
  return { nodes, input, focused, move };
}
test('picker/search arrows enter results and wrap without selecting or dispatching', () => {
  const f = fixture(); f.move('ArrowDown'); f.move('ArrowUp'); f.move('ArrowDown', f.nodes[2]); f.move('ArrowUp', f.nodes[0]);
  assert.deepEqual(f.focused, [0, 2, 0, 2]);
});
test('Home/End navigate result buttons but do not hijack text editing', () => {
  const f = fixture(); assert.equal(f.move('Home').prevented, undefined);
  f.move('Home', f.nodes[2]); f.move('End', f.nodes[0]); assert.deepEqual(f.focused, [0, 2]);
});
test('disabled and hidden results, input composition and modified arrows are ignored', () => {
  const f = fixture(); f.nodes[0].disabled = true; f.nodes[2].getClientRects = () => [];
  f.move('ArrowDown'); assert.deepEqual(f.focused, [1]);
  for (const flag of ['isComposing', 'altKey', 'ctrlKey', 'metaKey']) assert.equal(f.move('ArrowDown', f.input, { [flag]: true }).prevented, undefined);
  assert.deepEqual(f.focused, [1]);
});
test('the delivered dashboard and preview embed the queue/workspace before the application', () => {
  const root = path.join(__dirname, '..');
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote/dashboard.rs'), 'utf8');
  const preview = fs.readFileSync(path.join(root, 'scripts/dashboard-preview.py'), 'utf8');
  for (const source of ['dashboard-queue.js', 'dashboard-workspace.js']) { assert.ok(rust.includes(source)); assert.ok(preview.includes(source)); }
  assert.ok(rust.includes('{SESSION}\\n{QUEUE}\\n{WORKSPACE}\\n{SCRIPT}'));
});
test('all named controls exist once; no remote fonts, executable metadata or inline handlers', () => {
  const root = path.join(__dirname, '../crates/fsonos-api/src/remote');
  const html = fs.readFileSync(path.join(root, 'dashboard.html'), 'utf8');
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]);
  assert.equal(new Set(ids).size, ids.length);
  for (const name of ['rooms-dialog', 'tab-queue', 'tab-picks', 'queue-items', 'volume-panel', 'toggle-volume', 'dock-art', 'review-dialog']) assert.ok(ids.includes(name));
  assert.doesNotMatch(html, /on(?:click|load|error)=|<script(?![^>]*src=)[^>]*>/i);
  const css = fs.readFileSync(path.join(root, 'dashboard.css'), 'utf8');
  assert.doesNotMatch(css, /@import|https?:\/\//);
  const js = fs.readFileSync(path.join(root, 'dashboard-workspace.js'), 'utf8');
  assert.doesNotMatch(js, /innerHTML|outerHTML|insertAdjacentHTML|eval\(/);
});
