// node --test scripts/test-remote-studio.cjs
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const studio = require('../crates/fsonos-api/src/remote/studio.js');
const base = path.join(__dirname, '../crates/fsonos-api/src');
const zone = (household = 'S2', room = 'Den', transport_state = 'playing') => ({ household, coordinator_room: room, members: [room], transport_state });
const node = (label, id = '', summary = '') => ({ id, getAttribute: () => label, querySelector: () => ({ textContent: summary }) });

test('the selected household-qualified group survives reorder and duplicate names', () => {
  const s1 = zone('S1'), s2 = zone('S2');
  assert.equal(studio.choose([s1, s2], studio.key(s2)), studio.key(s2));
  assert.equal(studio.choose([s2, s1], studio.key(s2)), studio.key(s2));
  assert.notEqual(studio.key(s1), studio.key(s2));
});
test('a removed group cannot leave hidden stale controls selected', () => {
  const removed = zone('S2', 'Removed'), paused = zone('S2', 'Paused', 'paused'), playing = zone('S2', 'New');
  assert.equal(studio.choose([paused, playing], studio.key(removed)), studio.key(playing));
  assert.equal(studio.choose([paused], studio.key(removed)), studio.key(paused));
  assert.equal(studio.choose([], studio.key(removed)), null);
});
test('arbitrary room punctuation cannot collide in the selection key', () => {
  assert.notEqual(studio.key(zone('b@c', 'a')), studio.key(zone('c', 'a@b')));
  assert.equal(studio.choose([zone('S2', '<b>Den</b>')], null), '["S2","<b>Den</b>"]');
});
test('native music capabilities land in Music and DJ, not a settings pile', () => {
  for (const label of ['Music library', 'DJ session', 'DJ steering', 'DJ direction and explanations', 'DJ preferences', 'Play a link or stream', 'Spotify connection', 'Library sync']) {
    assert.equal(studio.identify(node(label)).workspace, 'music', label);
  }
});
test('grouping, sound, routines and recovery have deterministic destinations', () => {
  for (const [label, expected] of [['Room grouping', 'house'], ['Copy speaker sound', 'house'], ['Speaker sound', 'house'], ['Play mode', 'house'], ['Fades', 'routines'], ['Scenes', 'routines'], ['Sleep timers', 'routines'], ['Scheduled routines', 'routines'], ['Announcements', 'routines'], ['Action history', 'activity']]) {
    assert.equal(studio.identify(node(label)).workspace, expected, label);
  }
});
test('unknown future native panels remain reachable', () => {
  const found = studio.identify(node('New native capability'));
  assert.equal(found.workspace, 'house'); assert.equal(found.title, 'New native capability');
});
test('tool discovery uses native IDs and summaries when a label is unavailable', () => {
  assert.equal(studio.identify(node('', 'room-groups')).id, 'room-groups');
  assert.equal(studio.identify(node('', '', 'Sleep timers')).id, 'sleep');
});
test('the finder searches all words across capability descriptions and synonyms', () => {
  assert.ok(studio.search('whole work', studio.tools).some(t => t.id === 'dj-steering'));
  assert.ok(studio.search('move music', studio.tools).some(t => t.id === 'room-groups'));
  assert.ok(studio.search('  DINNER ', studio.tools).some(t => t.id === 'scenes'));
  assert.equal(studio.search('unmatched-feature', studio.tools).length, 0);
});
test('unmounted features cannot be returned by the command finder', () => {
  const available = studio.tools.filter(t => t.id === 'sleep');
  assert.equal(studio.search('DJ', available).length, 0);
  assert.deepEqual(studio.search('', available), available);
});
test('the presentation shell has no networking, synthetic playback state or command dispatch', () => {
  const source = fs.readFileSync(path.join(base, 'remote/studio.js'), 'utf8');
  for (const forbidden of ['fetch(', 'EventSource(', "call('", 'setInterval(', 'innerHTML', 'eval(']) assert.equal(source.includes(forbidden), false, forbidden);
  assert.ok(source.includes('sleeve.append(parts.art)'));
  assert.ok(source.includes('.append(node)'));
});
test('the index keeps native IDs and loads the studio after native controllers', () => {
  const html = fs.readFileSync(path.join(base, 'remote/index.html'), 'utf8');
  for (const id of ['pause-all','live','doctor','music-library','room-groups','scenes-section','scenes','households','toast']) {
    assert.equal(html.split(`id="${id}"`).length - 1, 1, id);
  }
  assert.ok(html.indexOf('/remote.js') < html.indexOf('/studio.js'));
  assert.ok(html.includes('<dialog')); assert.ok(html.includes('viewport-fit=cover'));
});
test('all workspace controls point to real static destinations', () => {
  const html = fs.readFileSync(path.join(base, 'remote/index.html'), 'utf8');
  for (const id of Object.keys(studio.workspaces)) {
    assert.ok(html.includes(`data-workspace="${id}"`));
    assert.ok(html.includes(`id="studio-${id}"`));
  }
});
test('studio assets use the same hardened asset helper and the route is registered', () => {
  const route = fs.readFileSync(path.join(base, 'http/remote/studio.rs'), 'utf8');
  assert.ok(route.includes('{SCRIPT}\\n{INSIGHTS_SCRIPT}'));
  assert.ok(route.includes('{STYLE}\\n{INSIGHTS_STYLE}'));
  assert.ok(fs.readFileSync(path.join(base, 'http/remote.rs'), 'utf8').includes('routes.extend(studio::routes(cx));'));
});
test('the stylesheet preserves native hidden state, reduced motion and visible keyboard focus', () => {
  const css = fs.readFileSync(path.join(base, 'remote/studio.css'), 'utf8');
  assert.match(css, /\[hidden\]\s*\{\s*display:\s*none\s*!important/);
  assert.ok(css.includes('prefers-reduced-motion'));
  assert.ok(css.includes(':focus-visible'));
  assert.ok(css.includes('safe-area-inset-bottom'));
});

test('the actual button-driven DJ direction root is a first-class steering tool', () => {
  assert.equal(studio.identify(node('DJ direction and explanations')).id, 'dj-steering');
});
