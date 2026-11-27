'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const Scenes = require('../crates/fsonos-api/src/remote/scenes.js');

const clone = value => structuredClone(value);
const track = (id = 'A', title = 'First movement') => ({ source_uri: `spotify:track:${id.repeat(22)}`, title });
const direction = () => ({ constraints: { include_artists: ['Earth, Wind & Fire', 'A name\nwith a line break'],
  include_genres: ['jazz'], decades: [1960], min_work_minutes: 2, max_work_minutes: 40,
  energy_bias: -1, allow_long_works: true, strict: true }, for_secs: 17 });
const scene = (extra = {}) => ({ name: 'Evening', saved: 1900000000, revision: 'scene-v1:evening-original',
  groups: [{ coordinator: 'Den', members: ['Hall@HH_S1'], source: { kind: 'dj', mood: 'focus', direction: direction() }, playing: false }],
  volumes: { Den: 28, 'Hall@HH_S1': 23 }, mutes: { Den: false, 'Hall@HH_S1': true }, ...extra });
const zones = () => [
  { household: 'HH_S1', coordinator_room: 'Den', members: ['Den', 'Hall'] },
  { household: 'HH_S1', coordinator_room: 'Kitchen', members: ['Kitchen'] },
  { household: 'HH_S2', coordinator_room: 'Office', members: ['Office'] },
  { household: 'HH_S2', coordinator_room: 'Study', members: ['Study'] },
];
function fixture(initial = scene()) {
  const f = { rows: [clone(initial)], zones: zones(), calls: [], hook: null,
    playlists: [{ id: 'SQ:7', title: 'Saved suite', art_uri: null }],
    moods: [{ name: 'focus' }, { name: 'calm' }] };
  f.resolve = target => {
    const rooms = f.zones.flatMap(zone => zone.members.map(room => ({ room, household: zone.household,
      player: `RINCON_${zone.household}_${room}`, coordinator: `RINCON_${zone.household}_${zone.coordinator_room}` })));
    const found = rooms.filter(room => target === room.room || target === `${room.room}@${room.household}` || target === room.player);
    if (found.length !== 1) throw new Error('The saved room is unavailable or ambiguous.');
    return { target, matched: 'room', rooms: found };
  };
  f.call = async (operation, options = {}) => {
    f.calls.push({ operation, ...clone(options) });
    if (f.hook) {
      const result = await f.hook(operation, options);
      if (result !== undefined) return result;
    }
    if (operation === 'scenes') return clone(f.rows);
    if (operation === 'scene') return clone(f.rows.find(row => row.name === options.params.name));
    if (operation === 'zones') return clone(f.zones);
    if (operation === 'sceneTargets') return clone(f.resolve(options.query.target));
    if (operation === 'moods') return { moods: clone(f.moods) };
    if (operation === 'sonosPlaylists') return clone(f.playlists);
    if (operation === 'sceneSave') {
      const old = f.rows.find(row => row.name === options.params.name);
      assert.equal(options.body.expected_revision, old?.revision ?? 'absent');
      const { expected_revision, ...definition } = clone(options.body);
      for (const group of definition.groups) {
        if (group.source.kind === 'sonos_playlist') {
          const identity = f.resolve(group.coordinator).rooms[0], saved = f.playlists.find(row => row.id === group.source.name);
          group.source.playlist = { household: identity.household, id: saved.id, title: saved.title };
          group.coordinator = `${identity.room}@${identity.household}`;
        }
        if (group.playing == null) group.playing = group.source.kind === 'keep' ? null : true;
      }
      const saved = { name: options.params.name, saved: 1900000001, revision: 'scene-v1:evening-edited', ...definition };
      f.rows = f.rows.filter(row => row !== old).concat(saved);
      return clone(saved);
    }
    throw new Error(`Unexpected operation ${operation}`);
  };
  f.control = Scenes.model({ call: f.call });
  f.writes = () => f.calls.filter(call => !['scenes', 'scene', 'zones', 'sceneTargets', 'moods', 'sonosPlaylists'].includes(call.operation));
  f.open = async () => { assert.equal(await f.control.refresh(), true); assert.equal(await f.control.beginEdit(initial.name), true, f.control.state.message); };
  f.save = async () => {
    assert.equal(await f.control.reviewDefinition(), true, f.control.state.message);
    const reviewed = f.control.state.pending;
    assert.equal(await f.control.confirm(reviewed), true, f.control.state.message);
    assert.equal(await f.control.confirm(reviewed), false);
    return f.writes()[0];
  };
  return f;
}

test('editing a saved DJ scene changes a room level and preserves every other authored value', async () => {
  const original = scene(), f = fixture(original); await f.open();
  assert.equal(f.control.state.name, 'Evening');
  assert.equal(f.control.state.editing.revision, original.revision);
  assert.deepEqual(f.control.state.draft.groups[0].originalDirection, direction());
  assert.equal(f.control.editRoom('Den@HH_S1', 'volumes', '31'), true);
  assert.deepEqual(f.writes(), []);
  const sent = await f.save();
  assert.equal(sent.operation, 'sceneSave');
  assert.deepEqual(sent.params, { name: 'Evening' });
  assert.deepEqual(sent.body, { expected_revision: original.revision, groups: original.groups,
    volumes: { ...original.volumes, Den: 31 }, mutes: original.mutes });
  assert.equal(f.writes().length, 1);
});

test('both households retain DJ direction, ordered duplicate tracks, keep playback and native playlist identity', async () => {
  const original = scene({ groups: [scene().groups[0],
    { coordinator: 'Office@HH_S2', members: [], source: { kind: 'tracks', tracks: [track('A', 'I. First\nmovement'), track('B'), track('A', 'Encore | ending')] }, playing: true },
    { coordinator: 'Study@HH_S2', members: [], source: { kind: 'keep' }, playing: null },
    { coordinator: 'Kitchen@HH_S1', members: [], source: { kind: 'sonos_playlist', name: 'SQ:7',
      playlist: { household: 'HH_S1', id: 'SQ:7', title: 'Saved suite' } }, playing: false }],
    volumes: { Den: 0, 'Office@HH_S2': 100 }, mutes: { 'Study@HH_S2': false } });
  const f = fixture(original); await f.open();
  assert.equal(f.control.editRoom('Study@HH_S2', 'mutes', 'muted'), true);
  const sent = await f.save();
  const groups = clone(original.groups);
  delete groups[2].playing; delete groups[3].source.playlist;
  assert.deepEqual(sent.body.groups, groups);
  assert.deepEqual(sent.body.volumes, original.volumes);
  assert.deepEqual(sent.body.mutes, { 'Study@HH_S2': true });
  assert.equal(f.control.state.saved.groups[1].source.tracks[0].title, 'I. First\nmovement');
  assert.deepEqual(f.control.state.saved.groups[3].source.playlist, original.groups[3].source.playlist);
});

test('editing one DJ filter preserves untouched multiline names and exact seconds', async () => {
  const f = fixture(); await f.open();
  assert.equal(f.control.editDirection(f.control.state.draft.groups[0], 'energy_bias', '2'), true);
  const sent = await f.save();
  assert.deepEqual(sent.body.groups[0].source.direction, { ...direction(),
    constraints: { ...direction().constraints, energy_bias: 2 } });
});

test('editing the mood preserves the existing exact direction and duration', async () => {
  const f = fixture(); await f.open();
  assert.equal(f.control.editGroup(f.control.state.draft.groups[0], 'value', 'calm'), true);
  const sent = await f.save();
  assert.equal(sent.body.groups[0].source.mood, 'calm');
  assert.deepEqual(sent.body.groups[0].source.direction, direction());
});

test('only an explicit duration edit replaces a second-precision expiry or clears it', async () => {
  for (const [minutes, seconds] of [['2', 120], ['', undefined]]) {
    const f = fixture(); await f.open();
    assert.equal(f.control.editDirection(f.control.state.draft.groups[0], 'minutes', minutes), true);
    const sent = await f.save();
    assert.deepEqual(sent.body.groups[0].source.direction.constraints, direction().constraints);
    assert.equal(sent.body.groups[0].source.direction.for_secs, seconds);
  }
});

test('legacy absent direction stays absent; explicitly empty direction stays an explicit reset', async () => {
  for (const source of [{ kind: 'dj' }, { kind: 'dj', direction: {} }]) {
    const f = fixture(scene({ groups: [{ coordinator: 'Den', members: [], source, playing: true }] }));
    await f.open(); f.control.editRoom('Den@HH_S1', 'volumes', '22');
    const sent = await f.save();
    assert.equal(Object.hasOwn(sent.body.groups[0].source, 'direction'), Object.hasOwn(source, 'direction'));
    if (source.direction) assert.deepEqual(sent.body.groups[0].source.direction, { constraints: {} });
  }
});

test('a legacy DJ scene can acquire explicit filters without starting the DJ', async () => {
  const f = fixture(scene({ groups: [{ coordinator: 'Den', members: [], source: { kind: 'dj' }, playing: false }] }));
  await f.open();
  f.control.editDirection(f.control.state.draft.groups[0], 'include_genres', 'ambient\njazz');
  f.control.editDirection(f.control.state.draft.groups[0], 'minutes', '90');
  const sent = await f.save();
  assert.deepEqual(sent.body.groups[0].source, { kind: 'dj', direction: { constraints: { include_genres: ['ambient', 'jazz'] }, for_secs: 5400 } });
  assert.equal(sent.body.groups[0].playing, false);
  assert.equal(f.writes().length, 1);
});

test('choosing Automatic DJ deliberately replaces legacy retained steering with a reusable program reset', async () => {
  const f = fixture(scene({ groups: [{ coordinator: 'Den', members: [], source: { kind: 'dj' }, playing: false }] }));
  await f.open();
  assert.equal(f.control.chooseSource(f.control.state.draft.groups[0], null), true);
  const sent = await f.save();
  assert.deepEqual(sent.body.groups[0].source, { kind: 'dj', direction: { constraints: {} } });
  assert.equal(sent.body.groups[0].playing, false);
});

test('membership and source replacements are authored deliberately while other groups remain exact', async () => {
  const f = fixture(); await f.open();
  f.control.editGroup(f.control.state.draft.groups[0], 'members', ['Hall@HH_S1', 'Kitchen@HH_S1']);
  f.control.editGroup(f.control.state.draft.groups[0], 'playing', 'play');
  const sent = await f.save();
  assert.deepEqual(sent.body.groups[0].members, ['Hall@HH_S1', 'Kitchen@HH_S1']);
  assert.equal(sent.body.groups[0].playing, true);
  assert.deepEqual(sent.body.groups[0].source, scene().groups[0].source);
});

test('volume and mute-only scenes can be edited without creating a capture request', async () => {
  const f = fixture(scene({ groups: [], volumes: { Den: 12 }, mutes: {} })); await f.open();
  f.control.editRoom('Den@HH_S1', 'mutes', 'unmuted');
  const sent = await f.save();
  assert.deepEqual(sent.body, { groups: [], volumes: { Den: 12 }, mutes: { 'Den@HH_S1': false }, expected_revision: scene().revision });
});

test('plain and scoped references to the same room retain their exact separate group, volume and mute roles', async () => {
  const original = scene({ volumes: { 'Den@HH_S1': 28, Hall: 23 }, mutes: { Den: false, 'Hall@HH_S1': true } });
  for (const edit of [false, true]) {
    const f = fixture(original); await f.open();
    if (edit) {
      f.control.editRoom('Den@HH_S1', 'volumes', '32');
      f.control.editRoom('Hall@HH_S1', 'mutes', 'unmuted');
      f.control.editGroup(f.control.state.draft.groups[0], 'playing', 'play');
    }
    const sent = await f.save();
    assert.deepEqual(sent.body, { expected_revision: original.revision,
      groups: [{ ...original.groups[0], playing: edit ? true : false }],
      volumes: { ...original.volumes, 'Den@HH_S1': edit ? 32 : 28 },
      mutes: { ...original.mutes, 'Hall@HH_S1': edit ? false : true } });
    assert.equal(f.writes().length, 1);
  }
});

test('the same physical room cannot occupy two group positions or two entries in one settings map', async () => {
  for (const original of [
    scene({ groups: [{ ...scene().groups[0], members: ['Den@HH_S1'] }] }),
    scene({ groups: [scene().groups[0], { ...scene().groups[0], coordinator: 'Den@HH_S1', members: [] }] }),
    scene({ volumes: { Den: 20, 'Den@HH_S1': 30 } }),
    scene({ mutes: { Den: false, 'Den@HH_S1': true } }),
  ]) {
    const f = fixture(original); await f.control.refresh();
    assert.equal(await f.control.beginEdit('Evening'), false);
    assert.match(f.control.state.message, /repeats a physical room/);
    assert.equal(f.control.state.draft, null);
    assert.deepEqual(f.writes(), []);
  }
});

test('removing an earlier group does not shift the surviving group’s original reference spellings', async () => {
  const original = scene({ groups: [
    { coordinator: 'Office@HH_S2', members: [], source: { kind: 'keep' }, playing: null },
    { coordinator: 'Den', members: ['Hall@HH_S1'], source: { kind: 'dj', direction: direction() }, playing: false },
  ], volumes: { 'Den@HH_S1': 28 }, mutes: {} });
  const f = fixture(original); await f.open();
  f.control.removeGroup(f.control.state.draft.groups[0]);
  const sent = await f.save();
  assert.deepEqual(sent.body.groups, [original.groups[1]]);
  assert.deepEqual(sent.body.volumes, original.volumes);
});

test('captured queue, backup, URI and favorite scenes refuse partial editing with a useful explanation', async () => {
  const queue = { coordinator: 'RINCON_HH_S1_Den', household: 'HH_S1', update_id: 7, track: 1, position_secs: 23, play_mode: 'NORMAL', boot_seq: 8 };
  for (const [source, reason] of [
    [{ kind: 'queue', queue }, /captured queue/],
    [{ kind: 'queue_backup', queue: { ...queue, total: 3 } }, /captured queue/],
    [{ kind: 'uri', uri: 'https://example.test/radio' }, /metadata/],
    [{ kind: 'favorite', name: 'Radio', uri: 'x-rincon-mp3radio:https://example.test/radio' }, /saved source identity/],
  ]) {
    const f = fixture(scene({ groups: [scene().groups[0], { coordinator: 'Office@HH_S2', members: [], source, playing: true }] }));
    await f.control.refresh(); const before = f.calls.length;
    assert.equal(await f.control.beginEdit('Evening'), false);
    assert.match(f.control.state.message, reason);
    assert.equal(f.control.state.draft, null);
    assert.equal(f.calls.length, before);
    assert.deepEqual(f.writes(), []);
  }
});

test('an uneditable implicit source playback state, empty scene or missing revision is not normalized into a different scene', async () => {
  for (const original of [scene({ groups: [{ ...scene().groups[0], playing: null }] }),
    scene({ groups: [], volumes: {}, mutes: {} }), scene({ revision: null })]) {
    const f = fixture(original); await f.control.refresh();
    assert.equal(await f.control.beginEdit('Evening'), false);
    assert.equal(f.control.state.draft, null);
    assert.deepEqual(f.writes(), []);
  }
});

test('a missing or ambiguous original room leaves the saved scene intact', async () => {
  for (const change of [f => { f.zones = f.zones.filter(zone => zone.household !== 'HH_S1'); },
    f => { f.zones.push({ household: 'HH_OTHER', coordinator_room: 'Den', members: ['Den'] }); }]) {
    const f = fixture(); await f.control.refresh(); change(f);
    assert.equal(await f.control.beginEdit('Evening'), false);
    assert.equal(f.control.state.draft, null);
    assert.deepEqual(f.writes(), []);
  }
});

test('the name stays fixed while editing so existing scene routines keep their target', async () => {
  const f = fixture(); await f.open();
  f.control.editName('Another scene');
  assert.equal(f.control.state.name, 'Evening');
  assert.equal(await f.control.beginDefinition(), false);
  f.control.discardDefinition();
  f.control.editName('Another scene');
  assert.equal(f.control.state.name, 'Another scene');
  assert.equal(f.control.state.editing, null);
  assert.deepEqual(f.rows, [scene()]);
});

test('an edit opens only the exact saved revision originally selected', async () => {
  const f = fixture(); await f.control.refresh();
  f.rows[0].revision = 'scene-v1:another-writer';
  assert.equal(await f.control.beginEdit('Evening'), false);
  assert.match(f.control.state.message, /changed/);
  assert.equal(f.control.state.draft, null);
  assert.deepEqual(f.writes(), []);
});

test('a concurrently edited scene blocks a reviewed replacement before the PUT', async () => {
  const f = fixture(); await f.open();
  f.control.editRoom('Den@HH_S1', 'volumes', '26');
  assert.equal(await f.control.reviewDefinition(), true);
  const reviewed = f.control.state.pending;
  f.rows[0].revision = 'scene-v1:changed-again'; f.rows[0].groups[0].source.direction.for_secs = 18;
  assert.equal(await f.control.confirm(reviewed), false);
  assert.deepEqual(f.writes(), []);
});

test('the original room reference is re-resolved during review and again before save', async () => {
  for (const afterReview of [false, true]) {
    const f = fixture(); await f.open();
    if (afterReview) assert.equal(await f.control.reviewDefinition(), true);
    const reviewed = f.control.state.pending;
    const resolve = f.resolve;
    f.resolve = target => { const value = resolve(target); if (target === 'Den') value.rooms[0].player = 'RINCON_REPLACEMENT'; return value; };
    assert.equal(afterReview ? await f.control.confirm(reviewed) : await f.control.reviewDefinition(), false);
    assert.match(f.control.state.message, /reference changed/);
    assert.deepEqual(f.writes(), []);
  }
});

test('a native playlist cannot silently change title or household while another field is edited', async () => {
  for (const changed of ['title', 'household']) {
    const original = scene({ groups: [{ coordinator: 'Kitchen@HH_S1', members: [], source: {
      kind: 'sonos_playlist', name: 'SQ:7', playlist: { id: 'SQ:7', title: 'Saved suite', household: changed === 'household' ? 'HH_S2' : 'HH_S1' } }, playing: false }] });
    const f = fixture(original); await f.open();
    if (changed === 'title') f.playlists[0].title = 'Other music';
    assert.equal(await f.control.reviewDefinition(), false);
    assert.match(f.control.state.message, /playlist identity changed/);
    assert.deepEqual(f.writes(), []);
  }
});

test('choosing a native replacement explicitly updates the saved reference after review', async () => {
  const original = scene({ groups: [{ coordinator: 'Kitchen@HH_S1', members: [], source: {
    kind: 'sonos_playlist', name: 'SQ:7', playlist: { id: 'SQ:7', title: 'Saved suite', household: 'HH_S1' } }, playing: false }] });
  const f = fixture(original); await f.open(); f.playlists[0].title = 'New suite';
  const group = f.control.state.draft.groups[0];
  assert.equal(await f.control.loadSources(group), true);
  assert.equal(f.control.chooseSource(group, f.control.choicesFor(group).items[0]), true);
  await f.save();
  assert.equal(f.control.state.saved.groups[0].source.playlist.title, 'New suite');
});

test('refreshing, discarding or closing cancels a pending edit read', async () => {
  for (const end of ['refresh', 'discardDefinition', 'close']) {
    const f = fixture(); await f.control.refresh();
    let release;
    f.hook = operation => operation === 'scene' ? new Promise(resolve => { release = resolve; }) : undefined;
    const pending = f.control.beginEdit('Evening');
    assert.equal(typeof release, 'function');
    await f.control[end](); release(clone(scene()));
    assert.equal(await pending, false);
    assert.equal(f.control.state.draft, null);
    assert.equal(f.control.state.editing, null);
    assert.deepEqual(f.writes(), []);
  }
});

test('a failed or malformed save is sent once and cannot reuse its review', async () => {
  for (const outcome of ['lost', 'wrong-direction']) {
    const f = fixture(); await f.open();
    assert.equal(await f.control.reviewDefinition(), true);
    const reviewed = f.control.state.pending;
    f.hook = operation => {
      if (operation !== 'sceneSave') return undefined;
      if (outcome === 'lost') throw new Error('Reply lost');
      const bad = scene(); bad.groups[0].source.direction.for_secs = 20; return bad;
    };
    assert.equal(await f.control.confirm(reviewed), false);
    assert.equal(await f.control.confirm(reviewed), false);
    assert.equal(f.writes().length, 1);
    assert.match(f.control.state.message, /uncertain.*not repeated/);
  }
});

test('an edited scene receipt without a valid revision stays uncertain after exactly one PUT', async () => {
  for (const revision of [undefined, null, '', 'absent', 'bad\nrevision']) {
    const f = fixture(); await f.open();
    assert.equal(await f.control.reviewDefinition(), true);
    const reviewed = f.control.state.pending;
    f.hook = operation => operation === 'sceneSave' ? scene({ revision }) : undefined;
    assert.equal(await f.control.confirm(reviewed), false);
    assert.equal(await f.control.confirm(reviewed), false);
    assert.equal(f.writes().length, 1);
    assert.equal(f.control.state.saved, null);
    assert.match(f.control.state.message, /uncertain.*not repeated/);
  }
});

function sceneDocument() {
  const listeners = new Map();
  const document = { activeElement: null,
    addEventListener(name, handler) { listeners.set(name, [...(listeners.get(name) || []), handler]); },
    removeEventListener(name, handler) { listeners.set(name, (listeners.get(name) || []).filter(item => item !== handler)); } };
  class Element {
    constructor(tag) {
      this.tagName = tag.toUpperCase(); this.children = []; this.attributes = new Map(); this.events = new Map();
      this.disabled = false; this.open = false; this.type = tag === 'input' ? 'text' : tag;
      this.value = ''; this.checked = false; this.selectionStart = 0; this.selectionEnd = 0;
      this.classList = { add: name => { this.className = `${this.className || ''} ${name}`.trim(); } };
    }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    set textContent(value) { this.children = [String(value)]; }
    get textContent() { return this.children.map(child => typeof child === 'string' ? child : child.textContent).join(''); }
    set innerHTML(_) { throw new Error('Scene values must remain text'); }
    setAttribute(name, value) {
      this.attributes.set(name, String(value));
      if (['disabled', 'open', 'checked'].includes(name)) this[name] = true;
      else if (['type', 'id'].includes(name) || name === 'value' && this.tagName !== 'TEXTAREA') this[name] = String(value);
    }
    getAttribute(name) { return this.attributes.get(name) ?? null; }
    contains(other) { return this === other || this.children.some(child => child instanceof Element && child.contains(other)); }
    descendants() { return this.children.flatMap(child => child instanceof Element ? [child, ...child.descendants()] : []); }
    querySelectorAll(selector) {
      const match = /^\[([^=\]]+)(?:="([^"]*)")?\]$/.exec(selector);
      return this.descendants().filter(child => match && child.attributes.has(match[1])
        && (match[2] === undefined || child.getAttribute(match[1]) === match[2]));
    }
    querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
    addEventListener(name, handler) { this.events.set(name, [...(this.events.get(name) || []), handler]); }
    async dispatch(name) { if (!this.disabled) for (const handler of this.events.get(name) || []) await handler({ target: this }); }
    focus() { document.activeElement = this; }
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
  }
  document.createElement = tag => new Element(tag);
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const helper = source.slice(source.indexOf('function el('), source.indexOf('\nlet toastTimer'));
  return { document, el: require('node:vm').runInNewContext(`${helper}\nel`, { document }) };
}

test('actual scene panel opens an existing DJ scene, changes playback and saves once through its buttons', async t => {
  const prior = global.document, dom = sceneDocument(); global.document = dom.document;
  t.after(() => { if (prior === undefined) delete global.document; else global.document = prior; });
  const f = fixture(), panel = Scenes.panel({ call: f.call, el: dom.el });
  const control = key => panel.node.querySelector(`[data-scene-focus="${key}"]`);
  panel.node.open = true; await control('refresh').dispatch('click');
  assert.equal(control('edit-Evening').disabled, false);
  await control('edit-Evening').dispatch('click');
  assert.match(panel.node.textContent, /Edit scene “Evening”/);
  assert.match(panel.node.textContent, /exact 17-second direction duration/);
  assert.match(panel.node.textContent, /Full saved names/);
  assert.equal(control('name').disabled, true);
  assert.equal(control('playing-1').value, 'pause');
  control('playing-1').value = 'play'; await control('playing-1').dispatch('change');
  assert.deepEqual(f.writes(), []);
  await control('review-definition').dispatch('click');
  assert.match(panel.node.textContent, /Replace custom scene “Evening”/);
  assert.equal(control('playing-1').disabled, true);
  await control('confirm').dispatch('click');
  assert.equal(f.writes().length, 1);
  assert.equal(f.writes()[0].body.groups[0].playing, true);
  assert.deepEqual(f.writes()[0].body.groups[0].source.direction, direction());
  panel.dispose();
});

test('actual scene cards explain unsupported captured-source editing and keep it disabled', async t => {
  const prior = global.document, dom = sceneDocument(); global.document = dom.document;
  t.after(() => { if (prior === undefined) delete global.document; else global.document = prior; });
  const f = fixture(scene({ groups: [{ coordinator: 'Den', members: [], source: { kind: 'uri', uri: 'https://example.test/radio' }, playing: true }] }));
  const panel = Scenes.panel({ call: f.call, el: dom.el });
  panel.node.open = true;
  await panel.node.querySelector('[data-scene-focus="refresh"]').dispatch('click');
  const edit = panel.node.querySelector('[data-scene-focus="edit-Evening"]');
  assert.equal(edit.disabled, true);
  assert.match(panel.node.textContent, /public scene view omits playback metadata/);
  await edit.dispatch('click');
  assert.deepEqual(f.writes(), []);
  panel.dispose();
});
