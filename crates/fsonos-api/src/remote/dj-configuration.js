// Owner-wide reusable moods and ordered daily programs. Writes use an exact
// revision; a lost reply never becomes an automatic retry or a replacement draft.
'use strict';
const DjConfiguration = (() => {
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const routes = { djConfigurationRead: ['GET', '/dj/configuration'], djConfigurationWrite: ['POST', '/dj/configuration'] };
  const days = Object.freeze(['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun']);
  const listKeys = [...Object.keys(D.lists), 'song_keywords'];
  const scalarKeys = ['min_work_minutes', 'max_work_minutes', 'energy_bias', 'allow_long_works', 'strict', 'expires_at'];
  const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
  const fail = message => { throw new Error(message); };
  const word = value => typeof value === 'string' && value.trim() !== '' && [...value.trim()].length <= 128;
  const problem = error => `${error?.message || 'The configuration request failed.'}${error?.hint ? ' ' + error.hint : ''}`;
  const freeze = value => {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) {
      for (const item of Object.values(value)) freeze(item);
      Object.freeze(value);
    }
    return value;
  };
  const copy = value => JSON.parse(JSON.stringify(value));
  const known = (value, fields, label) => {
    if (!object(value) || Object.keys(value).some(key => !fields.includes(key))) fail(`${label} contains an unreadable or unknown field. No settings were discarded.`);
  };

  function constraintsOf(value = {}) {
    known(value, [...listKeys, ...scalarKeys], 'The mood');
    const ordinary = Object.fromEntries(Object.entries(value).filter(([key]) => !['song_keywords', 'expires_at'].includes(key)));
    D.constraintsOf(ordinary);
    const songs = value.song_keywords === undefined ? [] : value.song_keywords;
    if (!Array.isArray(songs) || songs.length > 32 || songs.some(item => !word(item))) fail('Song keywords take at most 32 entries of 1–128 characters.');
    if (value.expires_at != null && !Number.isSafeInteger(value.expires_at)) fail('The mood expiry cannot be represented exactly. Use the current API or the owner’s configuration file.');
    const result = {};
    for (const key of listKeys) result[key] = [...(value[key] ?? [])];
    for (const key of ['periods', 'exclude_periods']) if (result[key].some(item => !D.periods.includes(item))) fail('A saved classical period is unreadable.');
    return freeze({ ...result, min_work_minutes: value.min_work_minutes ?? null,
      max_work_minutes: value.max_work_minutes ?? null, energy_bias: value.energy_bias ?? 0,
      allow_long_works: value.allow_long_works ?? false, strict: value.strict ?? false, expires_at: value.expires_at ?? null });
  }

  function moodMap(value) {
    if (!object(value)) fail('The mood catalogue is unreadable.');
    const names = Object.keys(value).sort(), seen = new Set();
    const entries = names.map(name => {
      if (!word(name) || name !== name.toLowerCase() || seen.has(name.toLowerCase())) fail('The mood catalogue contains an ambiguous name.');
      seen.add(name.toLowerCase());
      return [name, constraintsOf(value[name])];
    });
    return freeze(Object.fromEntries(entries));
  }

  function minute(value) {
    if (typeof value !== 'string' || !/^(?:[01]\d|2[0-3]):[0-5]\d$|^24:00$/.test(value)) fail('Program times use HH:MM, from 00:00 through 24:00.');
    const [hour, mins] = value.split(':').map(Number);
    return hour * 60 + mins;
  }

  function programsOf(value, moods) {
    if (!Array.isArray(value) || value.length > 256) fail('Use at most 256 ordered daily programs.');
    return freeze(value.map(row => {
      known(row, ['days', 'from', 'to', 'mood'], 'A daily program');
      if (!Array.isArray(row.days) || !row.days.length || row.days.length > 7
          || row.days.some(day => !days.includes(day)) || new Set(row.days).size !== row.days.length) fail('Choose one or more distinct starting days for each program.');
      minute(row.from); minute(row.to);
      if (!word(row.mood) || !Object.hasOwn(moods, row.mood)) fail(`The program mood “${row.mood ?? ''}” is unavailable. Restore it or edit that program.`);
      return { days: days.filter(day => row.days.includes(day)), from: row.from, to: row.to, mood: row.mood };
    }));
  }

  function configOf(value, builtins) {
    known(value, ['moods', 'programs'], 'The configuration');
    const moods = moodMap(value.moods);
    if (Object.keys(moods).length > 128) fail('Use at most 128 custom moods and overrides.');
    return freeze({ moods, programs: value.programs == null ? null : programsOf(value.programs, { ...builtins, ...moods }) });
  }

  const signature = value => JSON.stringify(value);
  function configurationOf(value) {
    if (!object(value) || typeof value.revision !== 'string' || !/^[0-9a-f]{64}$/.test(value.revision)) fail('The configuration revision is unreadable. Reload before editing.');
    const builtin_moods = moodMap(value.builtin_moods), moods = moodMap(value.moods);
    const builtin_programs = programsOf(value.builtin_programs, builtin_moods);
    const config = configOf(value.config, builtin_moods), programs = programsOf(value.programs, moods);
    const expected = moodMap({ ...builtin_moods, ...config.moods });
    if (signature(moods) !== signature(expected) || signature(config.programs ?? builtin_programs) !== signature(programs)) fail('The effective configuration is inconsistent. Reload before editing.');
    return freeze({ revision: value.revision, config, builtin_moods, builtin_programs, moods, programs });
  }

  // Endpoints are exclusive. A wrapping interval after midnight belongs to
  // yesterday’s selected start day; equal endpoints cover the selected day.
  function programAt(programs, day, time) {
    if (!days.includes(day)) fail('Choose a weekday for the preview.');
    const now = minute(time);
    if (now === 1440) fail('Preview a clock time before 24:00.');
    const previous = days[(days.indexOf(day) + 6) % 7];
    return programs.find(row => {
      const from = minute(row.from), to = minute(row.to);
      if (from === to) return row.days.includes(day);
      if (from < to) return row.days.includes(day) && now >= from && now < to;
      return (row.days.includes(day) && now >= from) || (row.days.includes(previous) && now < to);
    }) ?? null;
  }

  function moodDraft(value) {
    const original = constraintsOf(value), direction = { ...D.draft() };
    for (const key of Object.keys(D.lists)) direction[key] = original[key].join('\n');
    for (const key of ['min_work_minutes', 'max_work_minutes']) direction[key] = original[key] === null ? '' : String(original[key]);
    direction.energy_bias = String(original.energy_bias);
    direction.allow_long_works = original.allow_long_works; direction.strict = original.strict;
    return freeze({ original, direction, changed: [], song_keywords: original.song_keywords.join('\n'), expires_at: original.expires_at === null ? '' : String(original.expires_at) });
  }

  function editMoodDraft(draft, field, value) {
    let next;
    if (field === 'song_keywords' || field === 'expires_at') {
      if (typeof value !== 'string' || value.length > (field === 'song_keywords' ? 16384 : 24)) fail('That mood field is too long.');
      next = { ...draft, [field]: value };
    } else {
      if (field === 'minutes') fail('Reusable moods use an absolute expiry, not a session duration.');
      next = { ...draft, direction: D.editDraft(draft.direction, field, value) };
    }
    return freeze({ ...next, changed: [...new Set([...draft.changed, field])] });
  }

  function moodFromDraft(draft) {
    const touched = { ...D.draft() };
    for (const field of draft.changed) if (Object.hasOwn(touched, field)) touched[field] = draft.direction[field];
    const parsed = D.fromDraft(touched).constraints, defaults = constraintsOf();
    const value = { ...draft.original };
    for (const field of draft.changed) {
      if (field === 'song_keywords') value[field] = draft.song_keywords.split(/\r\n|\r|\n/).map(item => item.trim()).filter(Boolean);
      else if (field === 'expires_at') {
        if (draft.expires_at !== '' && !/^-?\d+$/.test(draft.expires_at)) fail('Expiry must be Unix seconds, or blank for no expiry.');
        value[field] = draft.expires_at === '' ? null : Number(draft.expires_at);
      } else value[field] = parsed[field] ?? defaults[field];
    }
    return constraintsOf(value);
  }

  function model({ call, changed = () => {}, canEdit = () => true, saved = () => {} }) {
    let epoch = 0, disposed = false, active = true, reading = null, attempted = null;
    const state = { open: false, loading: false, writing: false, base: null, latest: null, draft: null,
      selected: '', mood: null, stale: false, uncertain: false, pending: null, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const live = () => !disposed && active && state.open;
    const editable = () => live() && !!state.draft && !state.loading && !state.writing && canEdit();
    const dirty = () => !!state.draft && (signature(state.draft) !== signature(state.base.config) || !!state.mood?.changed.length);
    const effective = () => state.draft ? { ...state.base.builtin_moods, ...state.draft.moods } : {};
    const defaultPrograms = () => state.base?.builtin_programs ?? [];
    function preview() {
      if (!state.draft) return null;
      const candidate = state.mood?.changed.length ? { ...state.draft,
        moods: { ...state.draft.moods, [state.selected]: moodFromDraft(state.mood) } } : state.draft;
      const config = configOf(candidate, state.base.builtin_moods);
      return freeze({ config, moods: moodMap({ ...state.base.builtin_moods, ...config.moods }),
        programs: config.programs ?? state.base.builtin_programs });
    }
    function notifySaved(value) { try { saved(value); } catch (_) { /* The stored receipt remains authoritative. */ } }
    function accept(snapshot) {
      state.base = snapshot; state.latest = snapshot; state.draft = snapshot.config;
      state.stale = false; state.uncertain = false; attempted = null;
      state.selected = ''; state.mood = null; state.pending = null;
    }
    async function load({ discard = false } = {}) {
      if (!live() || state.writing) return false;
      const version = ++epoch;
      reading?.abort(); const abort = new AbortController(); reading = abort;
      state.loading = true; state.pending = null; state.message = 'Reading reusable moods and daily programs…'; publish();
      try {
        const snapshot = configurationOf(await call('/dj/configuration', undefined, { signal: abort.signal }));
        if (!live() || version !== epoch) return false;
        state.latest = snapshot;
        if (discard || !state.base || (!dirty() && !state.uncertain)) {
          accept(snapshot); state.message = 'Saved configuration loaded.';
        } else if (attempted && signature(snapshot.config) === signature(attempted.config)) {
          if (signature(state.draft) === signature(attempted.config) && !state.mood?.changed.length) {
            accept(snapshot); state.message = 'The saved configuration now matches your submitted draft.';
          } else {
            state.base = snapshot; state.stale = false; state.uncertain = false; attempted = null;
            state.message = 'Your submitted settings are saved. Your newer unsaved edits are retained.';
          }
          notifySaved(snapshot);
        } else if (snapshot.revision === state.base.revision) {
          state.stale = false; state.uncertain = false; attempted = null;
          state.message = 'Saved configuration is unchanged. Your draft is retained; review it before saving.';
        } else {
          state.stale = true; state.uncertain = false;
          state.message = 'Saved settings changed. Your draft is retained below. Compare both definitions, then explicitly discard the draft to load the latest settings.';
        }
        return true;
      } catch (error) {
        if (live() && version === epoch) { state.stale = true; state.message = problem(error); }
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function change(action) {
      if (!editable()) return false;
      state.pending = null;
      try { action(); state.message = ''; publish(); return true; }
      catch (error) { state.message = problem(error); publish(); return false; }
    }
    function stageMood() {
      if (!state.mood?.changed.length) return;
      const value = moodFromDraft(state.mood);
      state.draft = freeze({ ...state.draft, moods: moodMap({ ...state.draft.moods, [state.selected]: value }) });
      state.mood = moodDraft(value);
    }
    function review() {
      if (!editable() || state.stale || state.uncertain) return null;
      state.pending = null;
      try {
        stageMood();
        const config = configOf(state.draft, state.base.builtin_moods);
        if (signature(config) === signature(state.base.config)) fail('There are no configuration changes to save.');
        state.draft = config;
        state.pending = freeze({ expected_revision: state.base.revision, config });
        state.message = '';
      } catch (error) { state.message = problem(error); }
      publish(); return state.pending;
    }
    async function save(expected = state.pending) {
      if (!editable() || state.stale || state.uncertain || !expected || expected !== state.pending) return false;
      state.pending = null; state.writing = true; attempted = expected; ++epoch;
      state.message = 'Saving reusable moods and daily programs…'; publish();
      try {
        const answer = await call('/dj/configuration', expected);
        if (!object(answer) || !word(answer.done) || typeof answer.changed !== 'boolean') fail('The save acknowledgement is unreadable.');
        const snapshot = configurationOf(answer.configuration);
        if (signature(snapshot.config) !== signature(expected.config)) fail('The save acknowledgement does not match the submitted configuration.');
        if (disposed) return false;
        accept(snapshot); state.message = answer.done; notifySaved(snapshot); return true;
      } catch (error) {
        if (!disposed) {
          state.stale = true;
          state.uncertain = error?.code !== 'CONFIG_CHANGED';
          state.message = state.uncertain
            ? `${problem(error)} The outcome is uncertain. Your draft is retained. Check saved settings before another save; this request will not be repeated.`
            : 'Saved settings changed since this draft was read. Your draft is retained. Check saved settings to compare; nothing was overwritten.';
        }
        return false;
      } finally { state.writing = false; publish(); }
    }
    function close() { ++epoch; reading?.abort(); reading = null; state.loading = false; state.open = false; state.pending = null; publish(); }
    return { state, editable, dirty, effective, defaultPrograms, preview, load, review, save,
      canOpen: () => !disposed && active,
      async open() { if (disposed || !active) return false; state.open = true; publish(); return state.base ? true : load(); },
      select(name) { return change(() => { stageMood(); if (!Object.hasOwn(effective(), name)) fail('Choose an available mood.'); state.selected = name; state.mood = moodDraft(effective()[name]); }); },
      create(name) { return change(() => {
        stageMood(); if (typeof name !== 'string') fail('Enter a mood name.'); name = name.trim().toLowerCase();
        if (!word(name)) fail('Use a mood name of 1–128 characters.');
        if (Object.hasOwn(effective(), name)) fail('That mood already exists. Choose it to edit its definition.');
        if (Object.keys(state.draft.moods).length >= 128) fail('Use at most 128 custom moods and overrides.');
        const value = constraintsOf(); state.draft = freeze({ ...state.draft, moods: moodMap({ ...state.draft.moods, [name]: value }) });
        state.selected = name; state.mood = moodDraft(value);
      }); },
      editMood(field, value) { return change(() => { if (!state.mood) fail('Choose a mood first.'); state.mood = editMoodDraft(state.mood, field, value); }); },
      stage() { return change(stageMood); },
      remove(name) { return change(() => {
        if (state.selected !== name) stageMood();
        if (!Object.hasOwn(state.draft.moods, name)) fail('That mood already uses its built-in definition.');
        const moods = Object.fromEntries(Object.entries(state.draft.moods).filter(([key]) => key !== name));
        state.draft = freeze({ ...state.draft, moods }); state.selected = ''; state.mood = null;
      }); },
      programMode(mode) { return change(() => {
        if (!['default', 'disabled', 'custom'].includes(mode)) fail('Choose a program mode.');
        state.draft = freeze({ ...state.draft, programs: mode === 'default' ? null : mode === 'disabled' ? []
          : state.draft.programs?.length ? state.draft.programs : copy(state.base.builtin_programs) });
      }); },
      addProgram() { return change(() => {
        const rows = state.draft.programs ?? [];
        if (rows.length >= 256) fail('Use at most 256 daily programs.');
        const mood = Object.keys(effective())[0]; if (!mood) fail('Create a mood before adding a program.');
        state.draft = freeze({ ...state.draft, programs: [...rows, { days: [...days], from: '07:00', to: '10:00', mood }] });
      }); },
      editProgram(index, field, value) { return change(() => {
        if (!Number.isInteger(index) || !state.draft.programs?.[index] || !['days', 'from', 'to', 'mood'].includes(field)) fail('Choose an existing program field.');
        if (field === 'days' ? !Array.isArray(value) || value.some(day => !days.includes(day)) : typeof value !== 'string' || value.length > 128) fail('The program field is unreadable.');
        state.draft = freeze({ ...state.draft, programs: state.draft.programs.map((row, at) => at === index ? { ...row, [field]: copy(value) } : row) });
      }); },
      moveProgram(index, direction) { return change(() => {
        const rows = [...(state.draft.programs ?? [])], target = index + direction;
        if (!Number.isInteger(index) || ![-1, 1].includes(direction) || !rows[index] || !rows[target]) fail('Choose a neighboring position.');
        [rows[index], rows[target]] = [rows[target], rows[index]]; state.draft = freeze({ ...state.draft, programs: rows });
      }); },
      removeProgram(index) { return change(() => {
        if (!Number.isInteger(index) || !state.draft.programs?.[index]) fail('Choose an existing program.');
        state.draft = freeze({ ...state.draft, programs: state.draft.programs.filter((_, at) => at !== index) });
      }); },
      cancelReview() { if (!state.writing) { state.pending = null; publish(); } },
      close, suspend() { active = false; close(); }, resume() { if (!disposed) { active = true; publish(); } },
      dispose() { close(); disposed = true; active = false; },
    };
  }
  return Object.freeze({ routes, days, constraintsOf, moodMap, minute, programsOf, configOf, configurationOf,
    programAt, moodDraft, editMoodDraft, moodFromDraft, model });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjConfiguration;
