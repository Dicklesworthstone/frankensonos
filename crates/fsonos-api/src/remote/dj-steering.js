// Group-scoped steering and the DJ's own explanations; no inferred picks.
'use strict';

const DjSteeringRemote = (() => {
  const routes = { djSteeringZones: ['GET', '/zones'], djSteeringMoods: ['GET', '/dj/moods'],
    djSteeringStatus: ['GET', '/zones/{room}/dj'], djSteeringWrite: ['POST', '/dj/steer'],
    djSteeringStart: ['POST', '/dj/start'] };
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const { lists, periods } = D;
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const integer = (v, low, high) => Number.isInteger(v) && v >= low && v <= high;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const fail = (message) => { throw new Error(message); };
  const message = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The DJ request failed.';
  const word = (v) => text(v) && [...v.trim()].length <= 128;
  const strings = (v, limit = 100) => Array.isArray(v) && v.length <= limit && v.every((s) => typeof s === 'string');

  function zonesOf(value) {
    if (!Array.isArray(value)) fail('The room list is unreadable. Refresh DJ rooms.');
    const seen = new Set(), members = new Set();
    return value.map((z) => {
      if (!z || !text(z.household) || !text(z.coordinator_room) || !Array.isArray(z.members)
          || !z.members.length || !z.members.every(text) || !z.members.includes(z.coordinator_room)) {
        fail('The room list is incomplete. Refresh DJ rooms.');
      }
      const target = `${z.coordinator_room}@${z.household}`;
      if (seen.has(target) || !word(target)) fail('The DJ destination is ambiguous or too long.');
      seen.add(target);
      for (const room of z.members) {
        const key = JSON.stringify([z.household, room]);
        if (members.has(key)) fail('A room appears in more than one group. Refresh DJ rooms.');
        members.add(key);
      }
      return Object.freeze({ target, room: z.coordinator_room, label: `${z.members.join(' + ')} (${z.household})`,
        signature: JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]) });
    });
  }

  const constraintsOf = D.constraintsOf;

  function moodsOf(value) {
    if (!value || !Array.isArray(value.moods) || value.moods.length > 1000) fail('The DJ mood list is unreadable.');
    const names = value.moods.map((m) => word(m && m.name) ? m.name : fail('A DJ mood name is unreadable.'));
    if (new Set(names).size !== names.length) fail('The DJ mood list is ambiguous.');
    return Object.freeze(names);
  }

  function steeringOf(value) {
    if (!object(value) || !['session', 'program', 'none'].includes(value.source)
        || (value.mood != null && !word(value.mood))
        || (value.expires_at != null && !integer(value.expires_at, -8640000000000, 8640000000000))
        || (value.expires_in_secs != null && !integer(value.expires_in_secs, 0, Number.MAX_SAFE_INTEGER))) {
      fail('The current steering is unreadable. Refresh before editing.');
    }
    return Object.freeze({ source: value.source, mood: value.mood ?? null,
      constraints: constraintsOf(value.constraints), added: constraintsOf(value.added),
      expires_at: value.expires_at ?? null, expires_in_secs: value.expires_in_secs ?? null });
  }

  function workOf(w) {
    if (!object(w) || typeof w.composer !== 'string' || typeof w.title !== 'string'
        || !integer(w.movements, 0, 4294967295) || !object(w.reason) || typeof w.reason.summary !== 'string'
        || !strings(w.performers ?? []) || !strings(w.reason.relaxed ?? [])
        || !Array.isArray(w.reason.factors ?? []) || (w.reason.factors ?? []).length > 100
        || (w.album != null && typeof w.album !== 'string')
        || (w.minutes != null && !integer(w.minutes, 0, 4294967295))
        || (w.movement != null && !integer(w.movement, 1, w.movements))
        || (w.movement_title != null && typeof w.movement_title !== 'string')) fail('The DJ work explanation is unreadable.');
    const factors = (w.reason.factors ?? []).map((f) => {
      if (!f || !text(f.factor) || !integer(f.weight, -2147483648, 2147483647)) fail('A DJ selection factor is unreadable.');
      return Object.freeze({ factor: f.factor, weight: f.weight });
    });
    return Object.freeze({ composer: w.composer, title: w.title, movements: w.movements,
      performers: Object.freeze([...(w.performers ?? [])]), album: w.album ?? null, minutes: w.minutes ?? null,
      movement: w.movement ?? null, movement_title: w.movement_title ?? null,
      reason: Object.freeze({ summary: w.reason.summary, factors: Object.freeze(factors),
        relaxed: Object.freeze([...(w.reason.relaxed ?? [])]) }) });
  }

  function statusOf(value, zone) {
    if (!value || !zone || ![zone.room, zone.target].includes(value.zone) || typeof value.running !== 'boolean'
        || !Array.isArray(value.next ?? []) || (value.next ?? []).length > 2
        || (value.problem != null && typeof value.problem !== 'string')) fail('The DJ status is incomplete or belongs to another group.');
    return Object.freeze({ running: value.running, now: value.now == null ? null : workOf(value.now),
      next: Object.freeze((value.next ?? []).map(workOf)), steering: steeringOf(value.steering), problem: value.problem ?? null });
  }

  function requestOf(zone, draft, moods) {
    if (!word(zone) || !object(draft)) fail('Choose a DJ group and steering.');
    if (draft.clear === true) return Object.freeze({ zone, clear: true });
    if (draft.clear !== undefined && draft.clear !== false) fail('Clear must be a boolean.');
    if (draft.keep_expiry !== undefined && typeof draft.keep_expiry !== 'boolean') fail('Keep expiry must be a boolean.');
    if (draft.keep_expiry && draft.minutes !== undefined && draft.minutes !== '') fail('Preserving a deadline takes no new duration.');
    const body = { zone };
    if (draft.mood !== undefined && draft.mood !== '') {
      if (!word(draft.mood) || !moods.includes(draft.mood)) fail('Choose a mood returned by this daemon.');
      body.mood = draft.mood;
    }
    body.constraints = constraintsOf(draft.constraints);
    if (!body.mood && !Object.keys(body.constraints).length) fail('Choose a mood or a constraint, or explicitly clear steering.');
    if (draft.minutes !== undefined && draft.minutes !== '') {
      if (typeof draft.minutes !== 'string' || !/^\d+$/.test(draft.minutes) || !integer(Number(draft.minutes), 1, 10080)) {
        fail('Steering duration must be 1–10080 whole minutes, or blank for until cleared.');
      }
      body.for_secs = Number(draft.minutes) * 60;
    }
    return Object.freeze(body);
  }

  // Import explicit taste settings only, never a preview's start time, seed,
  // length or a command. A blank preview inherited live steering, not a draft.
  function settingsOf(value) {
    if (!object(value) || Object.keys(value).some((key) => !['mood', 'constraints'].includes(key))) {
      fail('A rehearsal draft can contain only a mood and temporary filters.');
    }
    if (value.mood !== undefined && !word(value.mood)) fail('The rehearsal mood is unreadable.');
    const constraints = constraintsOf(value.constraints);
    if (Object.values(constraints).some((items) => Array.isArray(items)
        && items.some((item) => typeof item === 'string' && /[\r\n]/.test(item)))) {
      fail('A filter name containing a line break cannot be edited without changing its meaning.');
    }
    if (!value.mood && !Object.keys(constraints).length) fail('This rehearsal inherited live steering; there are no temporary settings to copy.');
    return Object.freeze({ ...(value.mood ? { mood: value.mood } : {}), constraints });
  }

  // A decreasing countdown and changing tracks do not mean steering changed.
  const signatureOf = (s) => JSON.stringify([s.source, s.mood, s.constraints, s.added, s.expires_at]);

  function model({ call, changed = () => {} }) {
    let epoch = 0, disposed = false;
    const state = { open: false, loading: false, busy: false, ready: false, zones: [], moods: [],
      target: '', status: null, pending: null, imported: null, edited: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (v) => !disposed && state.open && v === epoch;
    const destination = () => state.zones.find((z) => z.target === state.target);
    const invalidate = () => { ++epoch; state.pending = null; state.status = null; state.imported = null;
      state.edited = false; state.ready = false; state.loading = false; };

    async function load(preferred = null, settings = null) {
      if (disposed) return false;
      state.open = true;
      if (state.busy) { publish(); return false; }
      const old = preferred || destination(); invalidate(); const v = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const [rawZones, rawMoods] = await Promise.all([call('djSteeringZones'), call('djSteeringMoods')]);
        const zones = zonesOf(rawZones), moods = moodsOf(rawMoods);
        if (!current(v)) return false;
        state.zones = zones; state.moods = moods;
        state.target = old && zones.some((z) => z.signature === old.signature) ? old.target : '';
        state.loading = false;
        if (state.target) return await select(state.target, settings);
        state.message = preferred ? 'The inspected group changed. Choose its current grouping before editing.'
          : zones.length ? 'Choose a group to read its DJ.' : 'No rooms discovered yet. Refresh after discovery.';
        return true;
      } catch (error) {
        if (current(v)) { state.zones = []; state.target = ''; state.message = message(error); }
        return false;
      } finally { if (current(v)) { state.loading = false; publish(); } }
    }

    async function openFor(zone, settings = null) {
      if (disposed || state.busy || !zone || !word(zone.target) || !text(zone.signature)) return false;
      if (settings !== null && state.open && (state.edited || state.pending)) {
        state.message = 'The steering editor has an unfinished draft. Close it or refresh DJ rooms to discard that draft before importing another.';
        publish(); return false;
      }
      let imported = null;
      try { if (settings !== null) imported = settingsOf(settings); }
      catch (error) { state.message = message(error); publish(); return false; }
      const requested = { target: zone.target, signature: zone.signature };
      if (!await load(requested, imported)) return false;
      return state.open && state.ready && state.target === requested.target
        && destination()?.signature === requested.signature;
    }

    async function select(target, settings = null) {
      if (disposed || !state.open || state.busy || !state.zones.some((z) => z.target === target)) return false;
      invalidate(); state.target = target; const v = epoch, zone = destination();
      state.loading = true; state.message = ''; publish();
      try {
        // Validate against this refresh's mood catalog, and copy before I/O.
        const imported = settings === null ? null : settingsOf(settings);
        if (imported) requestOf(target, imported, state.moods);
        const status = statusOf(await call('djSteeringStatus', { params: { room: target } }), zone);
        if (!current(v)) return false;
        state.status = status; state.imported = imported; state.ready = true;
        state.message = imported
          ? 'Rehearsal settings loaded as an unsaved draft. Choose an expiry and review before saving. This does not reproduce the exact rehearsed playlist.'
          : 'Last observed DJ state. Refresh to read changes from other controllers.';
        return true;
      } catch (error) { if (current(v)) state.message = message(error); return false; }
      finally { if (current(v)) { state.loading = false; publish(); } }
    }

    function review(draft, start = false) {
      if (disposed || !state.open || state.busy || !state.ready || state.loading) return null;
      state.pending = null;
      try {
        const body = requestOf(state.target, draft, state.moods);
        if (start && (body.clear || state.status.running)) fail('Start with settings requires a stopped DJ and a mood or constraints.');
        const preserve = !body.clear && draft.keep_expiry === true;
        const steering = state.status.steering;
        if (preserve && (steering.source !== 'session' || steering.expires_at === null
            || !integer(steering.expires_in_secs, 60, 604800))) fail('This deadline cannot be preserved. Choose a new duration or return to the program.');
        state.pending = Object.freeze({ body, start, keep_expiry: preserve,
          deadline: preserve ? steering.expires_at : null,
          zone: destination(), signature: signatureOf(steering) });
        state.message = '';
      } catch (error) { state.message = message(error); }
      publish(); return state.pending;
    }

    async function confirm(expected) {
      if (disposed || !state.open || state.busy || !state.ready || state.loading || !expected || expected !== state.pending) return false;
      const v = ++epoch; state.busy = true; state.pending = null; state.ready = false;
      state.message = 'Rechecking the group and its steering…'; publish();
      let sent = false, acknowledged = false, receipt = '';
      try {
        const zones = zonesOf(await call('djSteeringZones'));
        if (!current(v)) return false;
        if (!zones.some((z) => z.signature === expected.zone.signature)) fail('The group changed; nothing was saved. Refresh DJ rooms.');
        const before = statusOf(await call('djSteeringStatus', { params: { room: expected.zone.target } }), expected.zone);
        if (!current(v)) return false;
        if (signatureOf(before.steering) !== expected.signature) fail('The steering changed or expired; nothing was saved. Refresh and review again.');
        if (expected.start && before.running) fail('The DJ started since this review. Refresh and review its current session.');
        state.message = expected.start ? 'Starting the DJ with these settings…' : 'Saving steering…'; publish();
        if (!current(v)) return false;
        let body = expected.body;
        if (expected.keep_expiry) {
          // The phone's clock may be wrong. Use the freshly reported daemon
          // countdown rather than restarting the duration seen during review.
          const remaining = before.steering.expires_in_secs;
          if (!integer(remaining, 60, 604800)) fail('The deadline is too close to preserve. Refresh and choose a duration.');
          body = Object.freeze({ ...body, for_secs: remaining });
        }
        sent = true;
        const answer = await call(expected.start ? 'djSteeringStart' : 'djSteeringWrite', { body });
        if (!answer || !text(answer.done) || typeof answer.changed !== 'boolean'
            || !Array.isArray(answer.notes ?? []) || !(answer.notes ?? []).every((n) => n && typeof n.code === 'string' && typeof n.detail === 'string')) {
          fail('The steering save response is unreadable.');
        }
        acknowledged = true;
        if (disposed) return true;
        state.imported = null; state.edited = false;
        receipt = [answer.done, ...(answer.notes ?? []).map((n) => n.detail)].join(' ');
        state.message = receipt;
        if (current(v)) {
          const after = statusOf(await call('djSteeringStatus', { params: { room: expected.zone.target } }), expected.zone);
          if (current(v)) { state.status = after; state.ready = true; }
        }
        return true;
      } catch (error) {
        if (!disposed) state.message = acknowledged
          ? `${receipt} The fresh DJ status could not be read. ${message(error)} Refresh; do not resend.`
          : `${message(error)}${sent ? ' Outcome uncertain. Refresh; do not resend an uncertain save.' : ''}`;
        return acknowledged;
      } finally {
        state.busy = false;
        if (!sent && !current(v) && !disposed) state.message = 'Steering check canceled before saving. Refresh DJ rooms.';
        publish();
      }
    }
    function cancel() { if (!disposed && !state.busy) { state.pending = null; publish(); } }
    function edit() {
      if (!disposed && state.ready && !state.busy && !state.loading) {
        state.edited = true; state.pending = null; publish();
      }
    }
    function close() { if (!disposed) { invalidate(); state.open = false; publish(); } }
    function dispose() { close(); disposed = true; ++epoch; }
    return { state, load, openFor, select, review, confirm, cancel, edit, close, dispose };
  }

  let nextId = 0;
  function panel({ call, el }) {
    const id = `dj-steer-${++nextId}`;
    const button = (text, onclick) => el('button', { type: 'button', text, onclick });
    const toggle = button('DJ direction & why', () => { if (editor.state.open) editor.close(); else void editor.load(); });
    toggle.setAttribute('aria-controls', id);
    const box = el('div', { class: 'dj-direction-panel', id, hidden: true });
    const status = el('p', { class: 'dj-direction-status', role: 'status', 'aria-live': 'polite' });
    const refresh = button('Refresh DJ rooms', () => { void editor.load(); });
    const target = el('select', { id: `${id}-target` });
    const facts = el('div', { class: 'dj-direction-facts' });
    const importedNote = el('p', { class: 'dj-direction-warning', hidden: true,
      text: 'Unsaved rehearsal draft: only its mood and filters are copied. Choose an expiry, then review saving the settings or starting the DJ with them. The seed, start time and work list are not sent. Live choices may differ.' });
    const mood = el('select', { id: `${id}-mood` });
    const minutes = el('input', { id: `${id}-minutes`, type: 'number', min: 1, max: 10080, step: 1, placeholder: 'Until cleared' });
    const expiry = el('select', { id: `${id}-expiry` },
      el('option', { value: 'keep', text: 'Keep current deadline' }),
      el('option', { value: 'duration', text: 'New duration from saving' }),
      el('option', { value: 'until', text: 'Until cleared' }));
    const energy = el('select', { id: `${id}-energy` }, ...[-2, -1, 0, 1, 2].map((v, i) =>
      el('option', { value: v, text: ['Much calmer', 'Calmer', 'No extra shift', 'Brighter', 'Much brighter'][i] })));
    const long = el('input', { id: `${id}-long`, type: 'checkbox' });
    const min = el('input', { id: `${id}-min`, type: 'number', min: 1, max: 600, step: 1 });
    const max = el('input', { id: `${id}-max`, type: 'number', min: 1, max: 600, step: 1 });
    const filters = {}, filterBox = el('details', { class: 'dj-direction-filters' }, el('summary', { text: 'Composer, artist, genre, era and keyword filters' }));
    const filterGrid = el('div', { class: 'dj-direction-grid' });
    for (const [key, label] of Object.entries(lists)) {
      const input = el('textarea', { id: `${id}-${key}`, rows: 2, maxlength: 8256 }); filters[key] = input;
      filterGrid.append(el('div', {}, el('label', { for: input.id, text: label }), input));
    }
    filterBox.append(el('p', { text: 'One entry per line, up to 32. Commas stay in names. Decades: 1960, 1970, etc., on separate lines. Periods: ' + periods.join(', ') + '.' }), filterGrid);
    const reviewBox = el('div', { class: 'dj-direction-review', hidden: true, tabindex: -1 });
    const controls = [mood, expiry, minutes, energy, long, min, max, ...Object.values(filters)];
    function reviewSettings(start = false) {
      try {
        const constraints = { energy_bias: Number(energy.value), allow_long_works: long.checked };
        for (const [key, input] of Object.entries(filters)) {
          const lines = input.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
          constraints[key] = key === 'decades' || key === 'exclude_decades'
            ? lines.map((s) => /^\d+$/.test(s) ? Number(s) : fail('Enter one decade year per line.')) : lines;
        }
        for (const [key, input] of [['min_work_minutes', min], ['max_work_minutes', max]]) {
          if (input.value !== '') constraints[key] = /^\d+$/.test(input.value) ? Number(input.value) : fail('Work lengths need whole minutes.');
        }
        if (expiry.value === 'duration' && !minutes.value) fail('Enter the new duration in whole minutes.');
        editor.review({ mood: mood.value, minutes: expiry.value === 'duration' ? minutes.value : '',
          keep_expiry: expiry.value === 'keep', constraints }, start);
      } catch (error) { editor.cancel(); status.textContent = message(error); }
    }
    const review = button('Review steering', () => reviewSettings());
    const start = button('Review & start DJ', () => reviewSettings(true));
    const clear = button('Review return to program', () => editor.review({ clear: true }));
    controls.push(review, start, clear);
    let observed = null, displayedZones = null, displayedMoods = null, reviewed = null, imported = null;
    const editor = model({ call, changed: render });

    function describe(c) {
      return Object.entries(c).map(([key, value]) => `${lists[key] || key.replaceAll('_', ' ')}: ${Array.isArray(value) ? value.join('; ') : value}`).join(' · ') || 'No additional constraints';
    }
    function workNode(work, heading) {
      const node = el('section', { class: 'dj-direction-work' }, el('h3', { text: heading }),
        el('p', { text: `${work.composer}: ${work.title}` }),
        el('p', { text: [work.performers.join(', '), work.album, work.minutes === null ? '' : `${work.minutes} min`].filter(Boolean).join(' · ') }));
      if (work.movement !== null) node.append(el('p', { text: `Movement ${work.movement} of ${work.movements}${work.movement_title ? `: ${work.movement_title}` : ''}` }));
      node.append(el('p', { text: `Why: ${work.reason.summary}` }));
      if (work.reason.relaxed.length) node.append(el('p', { class: 'dj-direction-warning', text: `Filters relaxed by the DJ: ${work.reason.relaxed.join(', ')}` }));
      if (work.reason.factors.length) node.append(el('details', {}, el('summary', { text: 'Selection factors (multipliers, not probabilities)' }),
        el('ul', {}, ...work.reason.factors.map((f) => el('li', { text: `${f.factor}: ${f.weight / 1000}× (1× is neutral)` })))));
      return node;
    }
    function render(state) {
      box.hidden = !state.open; toggle.setAttribute('aria-expanded', String(state.open));
      refresh.disabled = state.busy; target.disabled = state.busy || state.loading || !state.zones.length;
      for (const control of controls) control.disabled = !state.ready || state.busy || state.loading;
      start.disabled = start.disabled || !state.status || state.status.running;
      status.textContent = state.loading ? 'Reading DJ state…' : state.message;
      if (displayedZones !== state.zones) {
        displayedZones = state.zones;
        target.replaceChildren(el('option', { value: '', text: 'Choose a group', disabled: true }),
          ...state.zones.map((z) => el('option', { value: z.target, text: z.label })));
      }
      target.value = state.target;
      if (displayedMoods !== state.moods) {
        displayedMoods = state.moods;
        mood.replaceChildren(el('option', { value: '', text: 'Follow time-of-day mood' }), ...state.moods.map((m) => el('option', { value: m, text: m })));
      }
      if (observed !== state.status) {
        observed = state.status; facts.replaceChildren();
        if (observed) {
          const s = observed.steering, c = s.source === 'session' ? s.added : {};
          facts.append(el('h3', { text: observed.running ? 'DJ running' : 'DJ not running' }),
            el('p', { text: `Steering source: ${s.source}${s.mood ? ` · Mood: ${s.mood}` : ''}` }),
            el('p', { text: `Effective filters: ${describe(s.constraints)}` }));
          if (s.source === 'session') facts.append(el('p', { text: s.expires_at === null ? 'Session lasts until cleared.' : `Session expires: ${new Date(s.expires_at * 1000).toLocaleString()}` }));
          if (observed.problem) facts.append(el('p', { class: 'dj-direction-warning', text: observed.problem }));
          if (observed.now) facts.append(workNode(observed.now, 'Current DJ work'));
          else facts.append(el('p', { text: 'No current DJ work reported.' }));
          observed.next.forEach((w, i) => facts.append(workNode(w, `Queued DJ work ${i + 1}`)));
          // Do not silently clear a mood removed from the daemon's catalog.
          if (s.source === 'session' && s.mood && !state.moods.includes(s.mood)
              && !Array.from(mood.options).some((option) => option.value === s.mood)) {
            mood.append(el('option', { value: s.mood, text: `${s.mood} (not listed; choose a mood)` }));
          }
          mood.value = s.source === 'session' ? s.mood || '' : '';
          expiry.value = s.source === 'session' && s.expires_at !== null ? 'keep' : 'until';
          expiry.options[0].disabled = s.source !== 'session' || s.expires_at === null;
          minutes.value = s.source === 'session' && s.expires_in_secs !== null ? String(Math.max(1, Math.ceil(s.expires_in_secs / 60))) : '';
          energy.value = String(c.energy_bias || 0); long.checked = c.allow_long_works || false;
          min.value = c.min_work_minutes ?? ''; max.value = c.max_work_minutes ?? '';
          for (const [key, input] of Object.entries(filters)) input.value = (c[key] || []).join('\n');
        }
      }
      importedNote.hidden = !state.imported;
      if (imported !== state.imported) {
        imported = state.imported;
        if (imported) {
          const c = imported.constraints;
          mood.value = imported.mood || '';
          energy.value = String(c.energy_bias || 0); long.checked = c.allow_long_works || false;
          min.value = c.min_work_minutes ?? ''; max.value = c.max_work_minutes ?? '';
          for (const [key, input] of Object.entries(filters)) input.value = (c[key] || []).join('\n');
          // Preview hours are not a live expiry. Require an explicit choice.
          expiry.value = 'duration'; minutes.value = '';
        }
      }
      minutes.disabled = !state.ready || state.busy || state.loading || expiry.value !== 'duration';
      reviewBox.hidden = !state.pending;
      if (reviewed !== state.pending) {
        reviewed = state.pending; reviewBox.replaceChildren();
        if (reviewed) {
          const pending = reviewed, body = pending.body;
          reviewBox.append(el('h3', { text: pending.zone.label }),
            el('p', { text: body.clear ? 'Clear this group’s session and return to its time-of-day program.'
              : `Replace this group’s session. Mood: ${body.mood || 'time-of-day'}. ${describe(body.constraints)}. Duration: ${pending.keep_expiry ? `keep deadline ${new Date(pending.deadline * 1000).toLocaleString()}` : body.for_secs ? `${body.for_secs / 60} minutes from saving` : 'until cleared'}.` }),
            el('p', { text: pending.start
              ? 'Starts the DJ with these settings from its first pick. This replaces the group’s current source and queue and lets the daemon keep choosing music.'
              : 'Applies from the next DJ pick, not to already queued works. Does not start, skip or restart playback. Standing preferences remain unchanged.' }),
            el('div', { class: 'row' }, button(pending.start ? 'Start DJ with these settings' : 'Save this group’s steering', () => { void editor.confirm(pending); }), button('Cancel', () => editor.cancel())));
          reviewBox.focus({ preventScroll: true });
        }
      }
    }
    target.addEventListener('change', () => { void editor.select(target.value); });
    for (const input of [mood, expiry, minutes, energy, long, min, max, ...Object.values(filters)]) {
      input.addEventListener('input', () => editor.edit()); input.addEventListener('change', () => editor.edit());
    }
    const field = (name, input) => el('div', {}, el('label', { for: input.id, text: name }), input);
    box.append(el('h2', { text: 'Direct this group’s DJ' }),
      el('div', { class: 'row' }, refresh, button('Close', () => { editor.close(); toggle.focus(); })),
      field('DJ group', target), status, importedNote, facts,
      el('p', { text: 'Edit the session below. Saving replaces its mood and extra filters. When the DJ is stopped, you can also start it with these settings. Keep the existing deadline, choose a new duration, or keep the session until cleared. Clearing returns to the program. Use DJ taste for persistent preferences across all groups.' }),
      el('div', { class: 'dj-direction-grid' }, field('Mood', mood), field('Expiry', expiry), field('New duration in minutes', minutes),
        field('Energy shift', energy), field('Allow long works', long), field('Minimum work minutes', min), field('Maximum work minutes', max)),
      filterBox, el('div', { class: 'row' }, review, start, clear), reviewBox);
    render(editor.state);
    return { node: el('div', { class: 'dj-direction' }, toggle, box), model: editor,
      openFor: async (zone, settings = null) => {
        const opened = await editor.openFor(zone, settings);
        if (editor.state.open && !editor.state.busy) {
          (opened && settings !== null ? expiry : target).focus(); box.scrollIntoView({ block: 'start' });
        }
        return opened;
      }, dispose: editor.dispose };
  }
  return { routes, lists: Object.freeze(lists), zonesOf, constraintsOf, moodsOf, steeringOf, workOf, statusOf, requestOf, settingsOf, signatureOf, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = DjSteeringRemote;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, DjSteeringRemote.routes);
  const directionMain = document.getElementById('households');
  if (directionMain) {
    const directionRoot = el('section', { class: 'scenes', 'aria-label': 'DJ direction and explanations' });
    const directionPanel = DjSteeringRemote.panel({ el, call: async (name, args) => {
      try { return await call(name, args); }
      finally {
        // A lost write answer is also a reason to re-observe, never to retry.
        if (name === 'djSteeringWrite' || name === 'djSteeringStart') document.dispatchEvent(new CustomEvent('fsonos:dj-changed', { detail: { zone: args.body.zone } }));
      }
    } });
    directionRoot.append(directionPanel.node);
    document.addEventListener('fsonos:dj-edit', (event) => {
      event.preventDefault();
      void directionPanel.openFor(event.detail, event.detail?.settings ?? null);
    });
    directionMain.before(directionRoot);
  }
}
