'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const M = require('../crates/fsonos-api/src/remote/dj-configuration.js');
const Panel = require('../crates/fsonos-api/src/remote/dj-configuration-panel.js');

const clone = value => structuredClone(value);
const revision = value => String(value).padStart(64, '0');
const builtinMoods = { calm: M.constraintsOf({ energy_bias: -1 }), focus: M.constraintsOf({ include_keywords: ['instrumental'] }) };
const builtinPrograms = [{ days: [...M.days], from: '00:00', to: '24:00', mood: 'calm' }];
function snapshot(config = { moods: {}, programs: null }, serial = 1) {
  config = M.configOf(config, builtinMoods);
  return clone({ revision: revision(serial), config, builtin_moods: builtinMoods, builtin_programs: builtinPrograms,
    moods: M.moodMap({ ...builtinMoods, ...config.moods }), programs: config.programs ?? builtinPrograms });
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture(initial = snapshot(), options = {}) {
  const calls = [], reads = [], writes = [], notices = [];
  let current = clone(initial), serial = 10;
  const call = async (url, body) => {
    calls.push({ url, body });
    assert.equal(url, '/dj/configuration');
    if (body === undefined) return reads.length ? reads.shift() : clone(current);
    if (writes.length) {
      const next = writes.shift(); return typeof next === 'function' ? next(body) : next;
    }
    if (body.expected_revision !== current.revision) throw Object.assign(new Error('Changed'), { code: 'CONFIG_CHANGED' });
    current = snapshot(body.config, ++serial);
    return { done: 'DJ configuration saved.', changed: true, configuration: clone(current) };
  };
  const editor = M.model({ ...options, saved: value => notices.push(value), call });
  return { editor, call, calls, reads, writes, notices, get current() { return current; }, set current(value) { current = clone(value); } };
}
const posts = fixture => fixture.calls.filter(call => call.body !== undefined);

test('full configuration reads retain every constraint, and unknown data cannot be dropped', () => {
  const full = M.constraintsOf({ include_artists: ['  Earth, Wind & Fire  '], exclude_artists: ['Another'],
    include_composers: ['Bach'], exclude_composers: ['Example'], include_genres: ['jazz'], exclude_genres: ['pop'],
    periods: ['baroque'], exclude_periods: ['modern'], song_keywords: ['gospel, soul'], decades: [1960], exclude_decades: [1980],
    include_keywords: ['piano'], exclude_keywords: ['vocal'], min_work_minutes: 2, max_work_minutes: 45,
    energy_bias: -2, allow_long_works: true, strict: true, expires_at: 2000000001 });
  const raw = snapshot({ moods: { evening: full }, programs: [] });
  const read = M.configurationOf(raw);
  assert.deepEqual(read.config.moods.evening, full);
  assert.ok(Object.isFrozen(read.config.moods.evening.song_keywords));
  raw.config.moods.evening.include_artists.push('Later');
  assert.deepEqual(read.config.moods.evening.include_artists, ['  Earth, Wind & Fire  ']);
  for (const change of [
    value => { value.config.future = true; },
    value => { value.config.moods.evening.future = true; },
    value => { value.config.moods.evening.song_keywords = null; },
    value => { value.config.moods.evening.expires_at = Number.MAX_SAFE_INTEGER + 1; },
    value => { value.revision = 'unversioned'; },
    value => { delete value.builtin_programs; },
    value => { value.moods.evening.energy_bias = 1; },
  ]) {
    const bad = clone(read); change(bad); assert.throws(() => M.configurationOf(bad));
  }
});

test('ordinary filter edits preserve untouched literal lists, song alternatives and exact expiry', () => {
  const original = M.constraintsOf({ include_artists: ['  Earth, Wind & Fire  '],
    song_keywords: ['gospel, soul', 'folk'], expires_at: 2000000001, strict: true });
  let draft = M.moodDraft(original);
  draft = M.editMoodDraft(draft, 'energy_bias', '2');
  assert.deepEqual(M.moodFromDraft(draft), { ...original, energy_bias: 2 });
  draft = M.editMoodDraft(draft, 'song_keywords', 'new soul\nfolk');
  draft = M.editMoodDraft(draft, 'expires_at', '');
  const edited = M.moodFromDraft(draft);
  assert.deepEqual(edited.song_keywords, ['new soul', 'folk']);
  assert.equal(edited.expires_at, null);
  assert.deepEqual(edited.include_artists, original.include_artists);
  assert.throws(() => M.moodFromDraft(M.editMoodDraft(draft, 'expires_at', '1e9')));
  assert.throws(() => M.moodFromDraft(M.editMoodDraft(draft, 'expires_at', '9007199254740992')));
});

test('missing and null programs follow defaults; an empty list disables them', () => {
  assert.equal(M.configOf({ moods: {} }, builtinMoods).programs, null);
  assert.equal(M.configOf({ moods: {}, programs: null }, builtinMoods).programs, null);
  assert.deepEqual(M.configOf({ moods: {}, programs: [] }, builtinMoods).programs, []);
  assert.deepEqual(M.configurationOf(snapshot()).programs, builtinPrograms);
  assert.deepEqual(M.configurationOf(snapshot({ moods: {}, programs: [] })).programs, []);
});

test('ambiguous names, unknown program fields, missing moods and invalid windows are rejected', () => {
  assert.throws(() => M.moodMap({ calm: {}, Calm: {} }));
  const row = { days: ['fri'], from: '21:00', to: '06:00', mood: 'calm' };
  for (const bad of [
    { ...row, days: [] }, { ...row, days: ['fri', 'fri'] }, { ...row, days: ['weekdays'] },
    { ...row, from: '24:01' }, { ...row, to: '6:00' }, { ...row, mood: 'missing' },
    { ...row, future: true },
  ]) assert.throws(() => M.programsOf([bad], builtinMoods));
  assert.equal(M.minute('24:00'), 1440);
});

test('first match wins; overnight windows belong to their starting day across week boundaries', () => {
  const overnight = { days: ['fri'], from: '21:00', to: '06:00', mood: 'focus' };
  const allDay = { days: [...M.days], from: '09:00', to: '09:00', mood: 'calm' };
  const rows = M.programsOf([overnight, allDay], builtinMoods);
  for (const [day, time, mood] of [
    ['fri', '20:59', 'calm'], ['fri', '21:00', 'focus'], ['sat', '05:59', 'focus'],
    ['sat', '06:00', 'calm'], ['fri', '03:00', 'calm'], ['sun', '23:00', 'calm'],
  ]) assert.equal(M.programAt(rows, day, time).mood, mood);
  assert.equal(M.programAt([...rows].reverse(), 'sat', '03:00').mood, 'calm');
  const sunday = [{ ...overnight, days: ['sun'] }];
  assert.equal(M.programAt(sunday, 'mon', '03:00').mood, 'focus');
  assert.equal(M.programAt(sunday, 'mon', '06:00'), null);
  assert.throws(() => M.programAt(rows, 'mon', '24:00'));
});

test('opening is one lazy owner-wide read, and closing retains all unsaved work', async () => {
  const f = fixture(); assert.equal(f.calls.length, 0);
  await f.editor.open();
  assert.equal(f.calls.length, 1); assert.equal(f.calls[0].body, undefined);
  f.editor.create('Evening Jazz'); f.editor.editMood('include_genres', 'jazz');
  f.editor.close(); await f.editor.open();
  assert.equal(f.calls.length, 1);
  assert.equal(f.editor.state.selected, 'evening jazz');
  assert.equal(f.editor.state.mood.direction.include_genres, 'jazz');
  assert.equal(posts(f).length, 0);
});

test('one exact CAS save carries the whole draft while retaining unrelated definitions', async () => {
  const original = M.constraintsOf({ song_keywords: ['gospel'], expires_at: 2000000001 });
  const f = fixture(snapshot({ moods: { existing: original }, programs: [] }));
  await f.editor.open(); f.editor.create('Night Jazz');
  f.editor.editMood('include_artists', 'Earth, Wind & Fire');
  f.editor.editMood('song_keywords', 'soul\njazz');
  f.editor.editMood('expires_at', '2100000000');
  f.editor.addProgram(); f.editor.editProgram(0, 'mood', 'night jazz');
  f.editor.editProgram(0, 'days', ['fri']); f.editor.editProgram(0, 'from', '21:00'); f.editor.editProgram(0, 'to', '06:00');
  const review = f.editor.review(); assert.ok(Object.isFrozen(review.config.moods['night jazz']));
  assert.equal(review.expected_revision, revision(1));
  assert.deepEqual(review.config.moods.existing, original);
  assert.equal(await f.editor.save(review), true);
  assert.deepEqual(posts(f).map(call => call.body), [review]);
  assert.deepEqual(f.editor.state.draft.moods['night jazz'].song_keywords, ['soul', 'jazz']);
  assert.equal(f.editor.state.draft.moods['night jazz'].expires_at, 2100000000);
  assert.equal(f.editor.dirty(), false); assert.equal(f.notices.length, 1);
});

test('a program-only save preserves legacy spaced names and filters and accepts its exact receipt', async () => {
  const legacy = M.constraintsOf({ include_keywords: [' piano '], song_keywords: [' gospel '], expires_at: 2000000001 });
  const f = fixture(snapshot({ moods: { ' evening ': legacy },
    programs: [{ days: ['fri'], from: '21:00', to: '06:00', mood: ' evening ' }] }));
  await f.editor.open(); f.editor.programMode('disabled');
  const review = f.editor.review();
  assert.deepEqual(review.config.moods, { ' evening ': legacy });
  assert.equal(await f.editor.save(review), true);
  assert.equal(f.editor.state.uncertain, false);
  assert.equal(f.editor.state.stale, false);
  assert.deepEqual(f.editor.state.base.config.moods, { ' evening ': legacy });
  assert.deepEqual(posts(f)[0].body.config.programs, []);
});

test('selection stages another mood without losing its uncommitted form fields', async () => {
  const f = fixture(); await f.editor.open(); f.editor.select('calm'); f.editor.editMood('strict', true);
  assert.equal(f.editor.select('focus'), true);
  assert.equal(f.editor.state.draft.moods.calm.strict, true);
  assert.equal(f.editor.preview().moods.calm.strict, true);
});

test('restore default uses server definitions and keeps dependent program references', async () => {
  const f = fixture(snapshot({ moods: { calm: { energy_bias: 2 } },
    programs: [{ days: ['mon'], from: '00:00', to: '24:00', mood: 'calm' }] }));
  await f.editor.open(); assert.equal(f.editor.remove('calm'), true);
  assert.deepEqual(f.editor.preview().moods.calm, builtinMoods.calm);
  assert.equal(f.editor.preview().programs[0].mood, 'calm');
  assert.ok(f.editor.review());
  assert.equal(f.editor.create('CALM'), false);
  f.editor.programMode('default');
  assert.deepEqual(f.editor.preview().programs, builtinPrograms);
});

test('removing a custom mood retains invalid dependent rows for explicit repair', async () => {
  const f = fixture(snapshot({ moods: { custom: {} }, programs: [{ days: ['mon'], from: '00:00', to: '24:00', mood: 'custom' }] }));
  await f.editor.open(); f.editor.remove('custom');
  assert.equal(f.editor.review(), null); assert.match(f.editor.state.message, /unavailable/);
  assert.equal(f.editor.state.draft.programs[0].mood, 'custom');
  f.editor.editProgram(0, 'mood', 'calm'); assert.ok(f.editor.review());
  assert.equal(posts(f).length, 0);
});

test('time window order and incomplete field drafts survive edits until validated', async () => {
  const f = fixture(snapshot({ moods: {}, programs: [] })); await f.editor.open();
  f.editor.addProgram(); f.editor.addProgram(); f.editor.editProgram(1, 'mood', 'focus');
  f.editor.moveProgram(1, -1); assert.equal(f.editor.state.draft.programs[0].mood, 'focus');
  f.editor.editProgram(0, 'from', '2'); assert.equal(f.editor.review(), null);
  assert.equal(f.editor.state.draft.programs[0].from, '2');
  f.editor.editProgram(0, 'from', '21:00'); f.editor.editProgram(0, 'to', '06:00');
  assert.ok(f.editor.review()); f.editor.removeProgram(1);
  assert.equal(f.editor.state.pending, null); assert.equal(f.editor.state.draft.programs.length, 1);
});

test('slower superseded reads and hidden reads cannot replace the current draft', async () => {
  const f = fixture(), slow = deferred(); f.reads.push(slow.promise, snapshot({ moods: { current: {} }, programs: [] }, 2));
  const first = f.editor.open(); await f.editor.load();
  slow.resolve(snapshot({ moods: { obsolete: {} }, programs: null }, 1));
  assert.equal(await first, false); assert.ok(Object.hasOwn(f.editor.state.draft.moods, 'current'));
  const hidden = deferred(); f.reads.push(hidden.promise); const reading = f.editor.load(); f.editor.suspend();
  hidden.resolve(snapshot({ moods: { hidden: {} }, programs: null }, 3));
  assert.equal(await reading, false); assert.ok(Object.hasOwn(f.editor.state.draft.moods, 'current'));
  f.editor.resume(); assert.equal(f.calls.length, 3);
});

test('edits invalidate old reviews, and double clicks never duplicate an in-flight save', async () => {
  const f = fixture(), delayed = deferred(); await f.editor.open(); f.editor.create('custom');
  const old = f.editor.review(); f.editor.editMood('strict', true);
  assert.equal(await f.editor.save(old), false);
  const current = f.editor.review(); f.writes.push(delayed.promise);
  const saving = f.editor.save(current);
  assert.equal(await f.editor.save(current), false); assert.equal(f.editor.editMood('strict', false), false);
  f.editor.close(); await f.editor.open(); assert.equal(await f.editor.load(), false);
  delayed.resolve({ done: 'Saved.', changed: true, configuration: snapshot(current.config, 2) });
  assert.equal(await saving, true); assert.equal(posts(f).length, 1);
});

test('409 retains the draft and its original CAS revision until an explicit discard reload', async () => {
  const f = fixture(); await f.editor.open(); f.editor.create('mine');
  f.current = snapshot({ moods: { theirs: {} }, programs: [] }, 2);
  assert.equal(await f.editor.save(f.editor.review()), false);
  assert.equal(f.editor.state.uncertain, false); assert.equal(f.editor.state.stale, true);
  assert.ok(Object.hasOwn(f.editor.state.draft.moods, 'mine'));
  assert.equal(f.calls.length, 2, 'a failed save must not trigger another request');
  await f.editor.load();
  assert.equal(f.editor.state.base.revision, revision(1));
  assert.ok(Object.hasOwn(f.editor.state.latest.config.moods, 'theirs'));
  assert.equal(f.editor.review(), null);
  await f.editor.load({ discard: true });
  assert.equal(f.editor.state.base.revision, revision(2));
  assert.ok(Object.hasOwn(f.editor.state.draft.moods, 'theirs'));
  assert.equal(Object.hasOwn(f.editor.state.draft.moods, 'mine'), false);
  assert.equal(posts(f).length, 1);
});

test('lost acknowledgement is resolved by explicit readback without replaying the write', async () => {
  const f = fixture(); await f.editor.open(); f.editor.create('mine');
  f.writes.push(body => { f.current = snapshot(body.config, 2); throw new Error('Connection lost'); });
  assert.equal(await f.editor.save(f.editor.review()), false);
  assert.equal(f.editor.state.uncertain, true); assert.equal(f.editor.review(), null);
  assert.equal(f.calls.length, 2);
  await f.editor.load();
  assert.equal(f.editor.state.uncertain, false); assert.equal(f.editor.dirty(), false);
  assert.equal(f.editor.state.base.revision, revision(2)); assert.equal(posts(f).length, 1);
});

test('readback of a lost save preserves newer edits made to the retained draft', async () => {
  const f = fixture(); await f.editor.open(); f.editor.create('mine');
  f.writes.push(body => { f.current = snapshot(body.config, 2); throw new Error('Lost reply'); });
  await f.editor.save(f.editor.review()); f.editor.editMood('energy_bias', '2');
  await f.editor.load();
  assert.equal(f.editor.state.uncertain, false); assert.equal(f.editor.dirty(), true);
  assert.equal(f.editor.preview().moods.mine.energy_bias, 2);
  assert.equal(f.editor.state.base.config.moods.mine.energy_bias, 0);
  assert.equal(f.editor.review().expected_revision, revision(2)); assert.equal(posts(f).length, 1);
});

test('failed readback retains uncertainty and rejects malformed save acknowledgements', async () => {
  const f = fixture(); await f.editor.open(); f.editor.create('mine');
  f.writes.push({ done: 'Saved.', changed: true, configuration: snapshot({ moods: { wrong: {} }, programs: null }, 2) });
  assert.equal(await f.editor.save(f.editor.review()), false);
  f.reads.push({}); assert.equal(await f.editor.load(), false);
  assert.equal(f.editor.state.uncertain, true); assert.ok(Object.hasOwn(f.editor.state.draft.moods, 'mine'));
  assert.equal(f.editor.review(), null); assert.equal(posts(f).length, 1);
});

test('ordinary object-property mood names cannot mutate map prototypes or disappear', async () => {
  const f = fixture(); await f.editor.open();
  assert.equal(f.editor.create('__proto__'), true); f.editor.editMood('strict', true);
  assert.equal(f.editor.create('constructor'), true);
  const config = f.editor.review().config;
  assert.equal(Object.hasOwn(config.moods, '__proto__'), true);
  assert.equal(config.moods.__proto__.strict, true);
  assert.equal(Object.getPrototypeOf(config.moods), Object.prototype);
  assert.equal(await f.editor.save(), true);
});

test('read-only presentation cannot stage, review or send changes', async () => {
  const f = fixture(snapshot(), { canEdit: () => false }); await f.editor.open();
  assert.equal(f.editor.create('mine'), false); assert.equal(f.editor.programMode('disabled'), false);
  assert.equal(f.editor.review(), null); assert.equal(await f.editor.save(), false);
  assert.deepEqual(f.editor.preview().programs, builtinPrograms); assert.equal(posts(f).length, 0);
});

test('disposal ignores a late save acknowledgement and never publishes another callback', async () => {
  let changed = 0;
  const f = fixture(snapshot(), { changed: () => changed++ }); await f.editor.open(); f.editor.create('mine');
  const review = f.editor.review(), wait = deferred(); f.writes.push(wait.promise);
  const saving = f.editor.save(review); f.editor.dispose(); const before = changed;
  wait.resolve({ done: 'Saved.', changed: true, configuration: snapshot(review.config, 2) });
  assert.equal(await saving, false); assert.equal(changed, before); assert.equal(f.notices.length, 0);
  assert.equal(await f.editor.open(), false); assert.equal(posts(f).length, 1);
});

// Exercise the actual component handlers without an optional browser binary.
// This models DOM ownership, focus and form values; viewport/layout checks live
// in test-dj-configuration-browser.cjs and are never inferred from these tests.
const descendants = node => node.children.flatMap(child => typeof child === 'object' ? [child, ...descendants(child)] : []);
function documentFixture() {
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map(), attrs = {};
    const node = { tagName: tag.toUpperCase(), children: [], parentNode: null, disabled: false, hidden: false,
      checked: false, open: false, _text: '', _value: undefined, selectionStart: null, selectionEnd: null,
      get id() { return attrs.id || ''; }, get className() { return attrs.class || ''; }, set className(value) { attrs.class = value; },
      get value() { return this._value ?? (this.tagName === 'SELECT' ? this.children.find(child => child.tagName === 'OPTION')?.value ?? '' : attrs.value ?? ''); },
      set value(value) { this._value = String(value); },
      get textContent() { return this._text + this.children.map(child => typeof child === 'string' ? child : child.textContent).join(' '); },
      set textContent(value) { this._text = String(value); this.replaceChildren(); },
      setAttribute(key, value) { attrs[key] = String(value); if (['disabled', 'hidden', 'checked', 'open'].includes(key)) this[key] = true; },
      getAttribute(key) { return Object.hasOwn(attrs, key) ? attrs[key] : null; },
      append(...children) { for (const child of children) { this.children.push(child); if (typeof child === 'object') child.parentNode = this; } },
      replaceChildren(...children) { for (const child of this.children) if (typeof child === 'object') child.parentNode = null; this.children = []; this.append(...children); },
      remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter(child => child !== this); this.parentNode = null; },
      querySelectorAll(selector) { const key = selector.slice(1, -1); return descendants(this).filter(child => child.getAttribute(key) !== null); },
      addEventListener(name, listener) { const list = listeners.get(name) || []; list.push(listener); listeners.set(name, list); },
      fire(name) { if (!this.disabled) for (const listener of listeners.get(name) || []) listener({ target: this, preventDefault() {} }); },
      focus() { doc.activeElement = this; }, setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
    };
    return node;
  } };
  return doc;
}
async function settled(check) {
  for (let index = 0; index < 200 && !check(); index++) await Promise.resolve();
  assert.ok(check(), 'the component reached the expected asynchronous boundary');
}

test('actual dialog handlers preserve input focus and complete mood data through a CAS save', async () => {
  const f = fixture(), doc = documentFixture();
  const ui = Panel.panel({ call: f.call, document: doc });
  const find = predicate => descendants(ui.node).find(predicate);
  const button = text => find(node => node.tagName === 'BUTTON' && node.textContent === text);
  const field = suffix => find(node => node.id.endsWith(suffix));
  assert.equal(f.calls.length, 0);
  button('Moods & daily programs').fire('click'); await settled(() => !!ui.control.state.base);
  assert.equal(find(node => node.tagName === 'DIALOG').open, true);
  field('-new-name').value = 'late-night'; button('Create mood').fire('click');
  let artists = field('-filters-include_artists');
  artists.value = 'Earth, Wind & Fire'; artists.focus(); artists.setSelectionRange(5, 5); artists.fire('input');
  const updated = field('-filters-include_artists');
  assert.notEqual(updated, artists); assert.equal(doc.activeElement, updated);
  assert.equal(updated.value, 'Earth, Wind & Fire'); assert.equal(updated.selectionStart, 5);
  field('-song-keywords').value = 'soul\ngospel'; field('-song-keywords').fire('input');
  field('-expiry').value = '2100000000'; field('-expiry').fire('input');
  field('-mode').value = 'custom'; field('-mode').fire('change');
  field('-from-0').value = '21:00'; field('-from-0').fire('input');
  field('-to-0').value = '06:00'; field('-to-0').fire('input');
  field('-program-mood-0').value = 'late-night'; field('-program-mood-0').fire('change');
  button('Add time window').fire('click');
  find(node => node.getAttribute('aria-label') === 'Move program 2 up').fire('click');
  assert.equal(field('-program-mood-1').value, 'late-night');
  button('Review changes').fire('click'); assert.equal(doc.activeElement.className, 'dj-config-review');
  button('Save for every DJ').fire('click'); await settled(() => !ui.control.state.writing);
  assert.equal(posts(f).length, 1); const body = posts(f)[0].body;
  assert.equal(body.expected_revision, revision(1));
  assert.deepEqual(body.config.moods['late-night'].include_artists, ['Earth, Wind & Fire']);
  assert.deepEqual(body.config.moods['late-night'].song_keywords, ['soul', 'gospel']);
  assert.equal(body.config.moods['late-night'].expires_at, 2100000000);
  assert.equal(body.config.programs[1].mood, 'late-night');
  assert.equal(ui.control.dirty(), false);
});

test('actual dialog keeps pending field edits through close and visibility suspension', async () => {
  const f = fixture(), doc = documentFixture(), ui = Panel.panel({ call: f.call, document: doc });
  await ui.control.open(); ui.control.select('calm'); ui.control.editMood('song_keywords', 'kept verbatim');
  ui.suspend(); assert.equal(ui.control.state.open, false);
  assert.equal(ui.control.state.mood.song_keywords, 'kept verbatim');
  ui.resume(); await ui.control.open();
  assert.equal(ui.control.state.mood.song_keywords, 'kept verbatim'); assert.equal(f.calls.length, 1);
  ui.control.review(); ui.control.close(); assert.equal(ui.control.state.pending, null);
  assert.equal(posts(f).length, 0);
});
