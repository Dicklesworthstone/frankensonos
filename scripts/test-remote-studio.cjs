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
test('the presentation shell leaves networking and speaker requests to native controllers', () => {
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

// Native control spies exercise the dock boundary without inventing another
// transport implementation. Real DOM/navigation coverage lives in the browser suite.
function dockHouse(group = zone()) {
  const copy = value => structuredClone(value);
  const calls = [];
  const target = `${group.coordinator_room}@${group.household}`;
  const parts = { zone: copy(group), lastState: { zone: copy(group), transport_state: group.transport_state },
    writing: false, disposed: false };
  for (const control of ['play', 'previous', 'next']) {
    parts[control] = { disabled: false, click() {
      calls.push({ control, target,
        verb: control === 'play' ? (parts.zone.transport_state === 'playing' ? 'pause' : 'resume') : control });
    } };
  }
  const selected = studio.key(group);
  return { parts, selected, calls,
    house: { zones: [copy(group)], cards: new Map([[selected, parts]]) } };
}

test('room search finds accented member names across household-qualified groups', () => {
  const shared = { ...zone('S2', 'Café'), members: ['Café', 'Office'] };
  const other = { ...zone('S1', 'Café'), members: ['Café', 'Terrace'] };
  const groups = [other, shared, zone('S2', 'Guest room')];
  assert.deepEqual(studio.matchingZones(groups, '  CAFE s2  '), [shared]);
  assert.deepEqual(studio.matchingZones(groups, 'office'), [shared]);
  assert.deepEqual(studio.matchingZones(groups, 's1 terrace'), [other]);
  assert.deepEqual(studio.matchingZones(groups, 'office s1'), []);
  assert.deepEqual(studio.matchingZones(groups, '  '), groups);
  assert.deepEqual(shared.members, ['Café', 'Office']);
});

test('dock activation invokes exactly the selected original handler and household', () => {
  const s1 = dockHouse(zone('S1', 'Den')), s2 = dockHouse(zone('S2', 'Den'));
  const house = { zones: [...s1.house.zones, ...s2.house.zones],
    cards: new Map([...s1.house.cards, ...s2.house.cards]) };
  for (const control of ['play', 'previous', 'next']) {
    assert.equal(studio.activate(house, s2.selected, control), true);
  }
  assert.deepEqual(s2.calls, [
    { control: 'play', target: 'Den@S2', verb: 'pause' },
    { control: 'previous', target: 'Den@S2', verb: 'previous' },
    { control: 'next', target: 'Den@S2', verb: 'next' },
  ]);
  assert.deepEqual(s1.calls, []);
});

test('dock delegates paused and stopped playback to the native resume handler', () => {
  for (const state of ['paused', 'stopped']) {
    const fixture = dockHouse(zone('S2', 'Den', state));
    assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), true);
    assert.deepEqual(fixture.calls, [{ control: 'play', target: 'Den@S2', verb: 'resume' }]);
  }
});

test('dock cannot bypass stale reads, command ownership or native disabled controls', () => {
  const cases = [
    ['invalidated playback', parts => { parts.lastState = null; }],
    ['native write pending', parts => { parts.writing = true; }],
    ['card disposed', parts => { parts.disposed = true; }],
    ['native control blocked', (parts, control) => { parts[control].disabled = true; }],
    ['native control absent', (parts, control) => { delete parts[control]; }],
    ['transitioning transport', parts => { parts.lastState.transport_state = 'transitioning'; }],
    ['unknown transport', parts => { parts.lastState.transport_state = 'unknown'; }],
    ['missing state identity', parts => { delete parts.lastState.zone; }],
  ];
  for (const [reason, invalidate] of cases) {
    for (const control of ['play', 'previous', 'next']) {
      const fixture = dockHouse();
      invalidate(fixture.parts, control);
      assert.equal(studio.activate(fixture.house, fixture.selected, control), false, `${reason}: ${control}`);
      assert.deepEqual(fixture.calls, [], `${reason}: ${control}`);
    }
  }
});

test('removed groups and stale cards cannot redirect the dock to a surviving room', () => {
  const fixture = dockHouse();
  fixture.house.zones = [zone('S1', 'Den')];
  assert.equal(studio.selectedCard(fixture.house, fixture.selected), null);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), false);
  fixture.house.zones = [zone()];
  fixture.house.cards.clear();
  assert.equal(studio.selectedCard(fixture.house, fixture.selected), null);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'next'), false);
  assert.deepEqual(fixture.calls, []);
});

test('dock rejects a card whose coordinator, household or membership changed', () => {
  const replacements = [
    zone('S1', 'Den'),
    zone('S2', 'Office'),
    { ...zone(), members: ['Den', 'Office'] },
  ];
  for (const changed of replacements) {
    const fixture = dockHouse();
    fixture.parts.zone = changed;
    assert.equal(studio.selectedCard(fixture.house, fixture.selected), null);
    assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), false);
    assert.deepEqual(fixture.calls, []);
  }
  const fixture = dockHouse();
  fixture.house.zones[0].members.push('Office');
  assert.equal(studio.selectedCard(fixture.house, fixture.selected), null);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), false);
  assert.deepEqual(fixture.calls, []);
});

test('a selected current card still requires playback observed for its exact group', () => {
  const observations = [
    zone('S1', 'Den'),
    zone('S2', 'Office'),
    { ...zone(), members: ['Den', 'Office'] },
    { ...zone(), members: null },
  ];
  for (const observed of observations) {
    const fixture = dockHouse();
    fixture.parts.lastState.zone = observed;
    assert.equal(studio.selectedCard(fixture.house, fixture.selected), fixture.parts);
    assert.equal(studio.activate(fixture.house, fixture.selected, 'next'), false);
    assert.deepEqual(fixture.calls, []);
  }
});

test('the dock waits when topology and playback disagree about the native toggle verb', () => {
  for (const [before, after] of [['playing', 'paused'], ['paused', 'playing']]) {
    const fixture = dockHouse(zone('S2', 'Den', before));
    // /zones can publish a new transport observation before the corresponding
    // /zones/{room} read finishes. The native toggle reads parts.zone.
    fixture.parts.zone.transport_state = after;
    fixture.house.zones[0].transport_state = after;
    assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), false);
    assert.deepEqual(fixture.calls, []);
    fixture.parts.lastState = { zone: structuredClone(fixture.parts.zone), transport_state: after };
    assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), true);
    assert.equal(fixture.calls[0].verb, after === 'playing' ? 'pause' : 'resume');
  }
});

test('membership ordering changes preserve the current control without mutating source data', () => {
  const fixture = dockHouse({ ...zone(), members: ['Office', 'Den'] });
  fixture.parts.zone.members = Object.freeze(['Den', 'Office']);
  fixture.house.zones[0].members = Object.freeze(['Office', 'Den']);
  fixture.parts.lastState.zone.members = Object.freeze(['Office', 'Den']);
  assert.equal(studio.selectedCard(fixture.house, fixture.selected), fixture.parts);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'previous'), true);
  assert.deepEqual(fixture.house.zones[0].members, ['Office', 'Den']);
  assert.deepEqual(fixture.parts.zone.members, ['Den', 'Office']);
});

test('each activation resolves the current native card instead of retaining an old handler', () => {
  const old = dockHouse(), replacement = dockHouse();
  old.house.cards.set(old.selected, replacement.parts);
  old.parts.disposed = true;
  assert.equal(studio.activate(old.house, old.selected, 'next'), true);
  assert.deepEqual(old.calls, []);
  assert.deepEqual(replacement.calls, [{ control: 'next', target: 'Den@S2', verb: 'next' }]);
});

test('a native handler retains single-flight ownership across repeated dock activation', () => {
  const fixture = dockHouse();
  fixture.parts.play.click = () => { fixture.calls.push('native play'); fixture.parts.writing = true; };
  assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), true);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'play'), false);
  assert.equal(studio.activate(fixture.house, fixture.selected, 'next'), false);
  assert.deepEqual(fixture.calls, ['native play']);
});

test('room selection helpers and unsupported dock actions never invoke native handlers', () => {
  const fixture = dockHouse();
  const selected = studio.choose(fixture.house.zones, null);
  assert.equal(studio.selectedCard(fixture.house, selected), fixture.parts);
  assert.deepEqual(studio.matchingZones(fixture.house.zones, 'Den'), fixture.house.zones);
  for (const control of ['volume', 'favorite', 'queue', 'dj', 'click', '__proto__', '', null]) {
    assert.equal(studio.activate(fixture.house, selected, control), false);
  }
  assert.deepEqual(fixture.calls, []);
});
