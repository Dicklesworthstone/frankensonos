// Inspect the DJ's actual picks and steering through the public API.
'use strict';

const DjSessionRemote = (() => {
  const routes = {
    sessionZones: ['GET', '/zones'],
    sessionStatus: ['GET', '/zones/{room}/dj'],
    sessionMoods: ['GET', '/dj/moods'],
    sessionRehearse: ['POST', '/dj/rehearse'],
  };
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const text = (v) => typeof v === 'string';
  const words = (v) => Array.isArray(v) && v.every(text);
  const integer = (v, min = 0, max = 4294967295) => Number.isSafeInteger(v) && v >= min && v <= max;
  const message = (error) => error && text(error.message)
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The DJ request failed.';
  const copy = (v) => JSON.parse(JSON.stringify(v));

  function zonesOf(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable.');
    const seen = new Set();
    return value.map((zone) => {
      if (!object(zone) || !text(zone.household) || !zone.household.trim()
          || !text(zone.coordinator_room) || !zone.coordinator_room.trim()
          || !words(zone.members) || !zone.members.length
          || !zone.members.every((room) => room.trim()) || !zone.members.includes(zone.coordinator_room)) {
        throw new Error('The room list is incomplete.');
      }
      for (const room of zone.members) {
        const key = JSON.stringify([zone.household, room]);
        if (seen.has(key)) throw new Error('A room belongs to more than one group. Refresh rooms.');
        seen.add(key);
      }
      return Object.freeze({ target: `${zone.coordinator_room}@${zone.household}`,
        room: zone.coordinator_room, label: `${zone.members.join(' + ')} (${zone.household})`,
        signature: JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) });
    });
  }

  function statusOf(value, zone) {
    const optional = (v, check) => v === undefined || v === null || check(v);
    const work = (v) => object(v) && text(v.title) && text(v.composer) && integer(v.movements, 1)
      && optional(v.performers, words) && optional(v.album, text)
      && optional(v.movement, (m) => integer(m, 1, v.movements)) && optional(v.movement_title, text)
      && optional(v.minutes, (m) => integer(m)) && object(v.reason) && text(v.reason.summary)
      && optional(v.reason.relaxed, words) && optional(v.reason.factors, (factors) => Array.isArray(factors)
        && factors.every((f) => object(f) && text(f.factor) && integer(f.weight, -2147483648, 2147483647)));
    if (!object(value) || value.zone !== zone.room || typeof value.running !== 'boolean'
        || !optional(value.now, work) || !optional(value.next, (next) => Array.isArray(next) && next.every(work))
        || !optional(value.problem, text) || !object(value.steering) || !text(value.steering.source)
        || !optional(value.steering.mood, text) || !optional(value.steering.constraints, object)
        || !optional(value.steering.added, object)
        || !optional(value.steering.expires_at, (at) => integer(at, -8640000000000, 8640000000000))
        || !optional(value.steering.expires_in_secs, (secs) => integer(secs, 0, Number.MAX_SAFE_INTEGER))) {
      throw new Error('The DJ observation is incomplete or belongs to a different coordinator. Refresh rooms.');
    }
    return copy(value);
  }

  const LABELS = {
    include_artists: 'Only artists', exclude_artists: 'Exclude artists',
    include_genres: 'Only genres', exclude_genres: 'Exclude genres',
    decades: 'Only decades', exclude_decades: 'Exclude decades',
    include_keywords: 'Include keywords', exclude_keywords: 'Exclude keywords',
    include_composers: 'Only composers', exclude_composers: 'Exclude composers',
    periods: 'Only classical periods', exclude_periods: 'Exclude classical periods',
    min_work_minutes: 'Minimum work minutes', max_work_minutes: 'Maximum work minutes',
    energy_bias: 'Energy shift', allow_long_works: 'Allow long works',
  };

  function constraintsText(value) {
    return Object.entries(value || {}).filter(([, item]) => item != null && item !== false
      && item !== 0 && (!Array.isArray(item) || item.length))
      .map(([key, item]) => `${LABELS[key] || key}: ${Array.isArray(item) ? item.join('; ')
        : object(item) ? JSON.stringify(item) : String(item)}`);
  }

  // A rehearsal is a read-only POST, not a plan to enqueue or start later.
  function rehearsalRequest(zone, draft = {}, moods = []) {
    if (!zone || !text(zone.target) || !zone.target.trim() || !object(draft)) {
      throw new Error('Choose a freshly observed DJ group first.');
    }
    const number = (value, fallback, min, max, label) => {
      if (value === undefined || value === '') return fallback;
      if (!(typeof value === 'number' || (text(value) && /^\d+$/.test(value)))
          || !integer(Number(value), min, max)) throw new Error(`${label} must be a whole number from ${min} to ${max}.`);
      return Number(value);
    };
    if (Object.keys(draft).some((key) => !['lengthMode', 'length', 'at', 'mood', 'seed'].includes(key))) {
      throw new Error('Unknown rehearsal setting.');
    }
    const body = { zone: zone.target };
    if (!['works', 'hours'].includes(draft.lengthMode ?? 'works')) throw new Error('Choose works or hours.');
    const hours = draft.lengthMode === 'hours';
    body[hours ? 'hours' : 'works'] = number(draft.length, hours ? 2 : 12, 1, hours ? 48 : 500, hours ? 'Hours' : 'Works');
    body.seed = number(draft.seed, 1, 0, Number.MAX_SAFE_INTEGER, 'Seed');
    if (draft.at !== undefined && draft.at !== '') {
      if (!text(draft.at)) throw new Error('Start time must be HH:MM or RFC 3339 with an offset.');
      const at = draft.at.trim();
      if (at && !/^([01]\d|2[0-3]):[0-5]\d$/.test(at) && !rehearsalTime(at)) {
        throw new Error('Start time must be HH:MM or RFC 3339 with an offset.');
      }
      if (at) body.at = at; // HH:MM belongs to the daemon's clock, never the phone's.
    }
    if (draft.mood !== undefined && draft.mood !== '') {
      if (!text(draft.mood) || !moods.includes(draft.mood)) throw new Error('Choose a mood returned by this daemon.');
      body.mood = draft.mood;
    }
    return Object.freeze(body);
  }

  function rehearsalTime(value) {
    if (!text(value)) return false;
    const match = /^(\d{4})-(\d{2})-(\d{2})T([01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:\.\d+)?(?:Z|[+-]([01]\d|2[0-3]):[0-5]\d)$/.exec(value);
    if (!match || !Number.isFinite(Date.parse(value))) return false;
    const year = Number(match[1]), month = Number(match[2]), day = Number(match[3]);
    const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
    return month >= 1 && month <= 12 && day >= 1
      && day <= [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
  }

  function rehearsalOf(value, zone, request) {
    const optional = (v, check) => v == null || check(v);
    const energy = (v) => integer(v, 0, 100);
    const variety = (v) => Number.isFinite(v) && v >= 0;
    const bad = () => { throw new Error('The rehearsal is incomplete or belongs to a different request. Generate it again.'); };
    if (!object(value) || value.zone !== zone.room || !rehearsalTime(value.from)
        || !integer(value.seed, 0, Number.MAX_SAFE_INTEGER) || value.seed !== request.seed
        || !Array.isArray(value.works) || value.works.length > (request.works ?? 500)
        || !object(value.summary)) bad();
    if (request.at && (request.at.length === 5
      ? value.from.slice(11, 16) !== request.at : Date.parse(value.from) !== Date.parse(request.at))) bad();
    let previous = Date.parse(value.from);
    for (const row of value.works) {
      if (!object(row) || !rehearsalTime(row.at) || Date.parse(row.at) < previous
          || !energy(row.energy) || !optional(row.energy_target, energy)
          || !optional(row.genre, text) || !optional(row.mood, text) || typeof row.explicit !== 'boolean') bad();
      previous = Date.parse(row.at);
    }
    // Reuse the inspector's work/reason contract, including whole-work metadata.
    statusOf({ zone: zone.room, running: false, next: value.works.map((row) => row.work),
      steering: { source: 'none' } }, zone);
    const sum = value.summary;
    if (sum.works !== value.works.length || !integer(sum.minutes)
        || !integer(sum.distinct_artists, 0, sum.works) || !integer(sum.explicit, 0, sum.works)
        || sum.explicit !== value.works.filter((row) => row.explicit).length
        || !Array.isArray(sum.hours) || sum.hours.length > sum.works) bad();
    let count = 0;
    for (const hour of sum.hours) {
      if (!object(hour) || !rehearsalTime(hour.from) || !integer(hour.works, 1, sum.works)
          || !energy(hour.energy) || !optional(hour.energy_target, energy)
          || !variety(hour.artist_entropy) || !variety(hour.genre_entropy)
          || hour.from !== value.works[count]?.at) bad();
      count += hour.works;
    }
    if (count !== sum.works) bad();
    return copy(value);
  }

  function rehearsalModel({ call, changed = () => {}, now = Date.now }) {
    let epoch = 0, disposed = false;
    const state = { zone: null, open: false, busy: false, loadingMoods: false, moods: [],
      report: null, request: null, generatedAt: null, message: '', moodError: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && state.zone && version === epoch;
    function context(zone) {
      if (disposed || (zone?.signature === state.zone?.signature && zone?.target === state.zone?.target)) return;
      ++epoch;
      state.zone = zone ? Object.freeze({ ...zone }) : null;
      state.report = null; state.request = null; state.generatedAt = null; state.message = '';
      publish();
    }
    async function loadMoods() {
      if (disposed || state.loadingMoods) return false;
      state.loadingMoods = true; state.moodError = ''; publish();
      try {
        const value = await call('sessionMoods');
        if (!object(value) || !Array.isArray(value.moods) || value.moods.length > 1000
            || !value.moods.every((m) => object(m) && text(m.name) && m.name.trim())) {
          throw new Error('The mood list is unreadable.');
        }
        const names = value.moods.map((m) => m.name);
        if (new Set(names).size !== names.length) throw new Error('The mood list is ambiguous.');
        if (disposed) return false;
        state.moods = names;
        return true;
      } catch (error) {
        if (!disposed) { state.moods = []; state.moodError = `${message(error)} Current steering is still available.`; }
        return false;
      } finally { state.loadingMoods = false; publish(); }
    }
    function open() { if (!disposed) { state.open = true; publish(); } }
    function close() { ++epoch; state.open = false; publish(); }
    function dispose() { close(); disposed = true; }
    async function generate(draft) {
      if (disposed || !state.open || !state.zone || state.busy) return false;
      let request;
      try { request = rehearsalRequest(state.zone, draft, state.moods); }
      catch (error) { state.message = message(error); publish(); return false; }
      const zone = state.zone, version = ++epoch;
      state.busy = true; state.message = ''; state.report = null; state.generatedAt = null;
      state.request = request; publish();
      const verify = async () => {
        const zones = zonesOf(await call('sessionZones'));
        if (!current(version)) return false;
        if (!zones.some((z) => z.target === zone.target && z.signature === zone.signature)) {
          throw new Error('The group changed. Refresh DJ rooms and choose the group again.');
        }
        return true;
      };
      try {
        if (!await verify()) return false;
        const result = await call('sessionRehearse', { body: request });
        if (!current(version)) return false;
        const report = rehearsalOf(result, zone, request);
        if (!await verify()) return false;
        state.report = report; state.generatedAt = now();
        state.message = 'Rehearsal complete. Nothing was played, queued, or saved to listening history.';
        return true;
      } catch (error) {
        if (current(version)) state.message = error?.uncertain
          ? 'The rehearsal did not return a result. This endpoint only plans a set; it does not play or queue music. Generate again to retry.'
          : message(error);
        return false;
      } finally {
        // Closing or changing rooms does not free an unfinished request's slot.
        state.busy = false; publish();
      }
    }
    return { state, context, open, close, dispose, loadMoods, generate };
  }

  function rehearsalPanel({ call, el, workNode }) {
    let control, rendered = null;
    const node = el('details', { class: 'dj-rehearsal' });
    const mode = el('select', { 'aria-label': 'Rehearsal length type' },
      el('option', { value: 'works', text: 'Number of works' }), el('option', { value: 'hours', text: 'Hours of music' }));
    const length = el('input', { type: 'number', min: '1', max: '500', value: '12', required: true,
      'aria-label': 'Rehearsal length' });
    mode.addEventListener('change', () => { length.max = mode.value === 'hours' ? '48' : '500'; length.value = mode.value === 'hours' ? '2' : '12'; });
    const at = el('input', { type: 'text', placeholder: 'Now, or 23:00', maxLength: '64',
      'aria-label': 'Rehearsal start time', autocomplete: 'off' });
    const seed = el('input', { type: 'number', min: '0', max: String(Number.MAX_SAFE_INTEGER), value: '1',
      'aria-label': 'Rehearsal seed' });
    const mood = el('select', { 'aria-label': 'Rehearsal mood' });
    const reload = el('button', { type: 'button', text: 'Refresh moods', onclick: () => control.loadMoods() });
    const submit = el('button', { type: 'submit', text: 'Rehearse — do not play' });
    const info = el('p', { role: 'status', 'aria-live': 'polite' });
    const moodInfo = el('p', { class: 'by' });
    const output = el('div', { class: 'dj-rehearsal-results' });
    const fields = el('fieldset', {}, el('legend', { text: 'Plan a set, without changing the DJ' }),
      el('div', { class: 'dj-rehearsal-fields' },
        el('label', {}, 'Plan by', mode), el('label', {}, 'Length', length),
        el('label', {}, 'Start (house time)', at), el('label', {}, 'Seed', seed),
        el('label', {}, 'Steering for this rehearsal only', mood)), reload, submit);
    const form = el('form', { onsubmit: (event) => {
      event.preventDefault();
      void control.generate({ lengthMode: mode.value, length: length.value, at: at.value, mood: mood.value, seed: seed.value });
    } }, fields);
    node.append(el('summary', { text: 'Rehearse a set · hear the plan before you play' }),
      el('p', { text: 'Preview the DJ’s choices from your library, history, and preferences. No playback, queue edits, or saved steering. A time such as 23:00 uses the house’s clock, not this browser’s time zone. Blank starts now.' }),
      form, moodInfo, info, output);
    const clock = (value) => value.replace('T', ' '); // Keep the response's date and offset, including midnight.
    function render(state) {
      fields.disabled = !state.zone || state.busy || !state.open;
      form.setAttribute('aria-busy', String(state.busy));
      reload.disabled = state.loadingMoods;
      const selected = mood.value;
      mood.replaceChildren(el('option', { value: '', text: 'Current steering and time-of-day programs' }));
      for (const name of state.moods) mood.append(el('option', { value: name, text: name }));
      mood.value = state.moods.includes(selected) ? selected : '';
      moodInfo.textContent = state.loadingMoods ? 'Reading available moods…' : state.moodError;
      info.textContent = state.busy ? 'Planning the set… No music is being changed.' : state.message
        || (state.zone ? `For ${state.zone.label}.` : 'Choose a freshly observed group above.');
      if (rendered === state.report) return;
      rendered = state.report; output.replaceChildren();
      if (!state.report) return;
      const value = state.report, sum = value.summary;
      output.append(el('h4', { text: `Rehearsal for ${state.zone.label}` }),
        el('p', { class: 'by', text: `From ${clock(value.from)} · seed ${value.seed}. Snapshot of the library, history and preferences when generated, not a promise of future picks.` }),
        el('p', { text: `${sum.works} works · ${sum.minutes} minutes · ${sum.distinct_artists} artists · ${sum.explicit} explicit works` }));
      output.append(el('p', { class: 'by', text: `Requested: ${state.request.works == null ? `${state.request.hours} hours (up to 500 works)` : `${state.request.works} works`}; ${state.request.mood ? `mood ${state.request.mood}` : 'current steering and time-of-day programs'}.` }));
      if (state.request.hours && sum.works === 500) output.append(el('p', { text: 'The 500-work limit was reached. The requested time window may not be fully covered.' }));
      if (state.request.works && sum.works > 0 && sum.works < state.request.works) output.append(el('p', { text: 'The DJ returned fewer works than requested. This is the available set, not a full-length playlist.' }));
      if (!sum.works) output.append(el('p', { text: 'No works matched this rehearsal. Try another mood or sync your library.' }));
      if (sum.hours.length) {
        output.append(el('h4', { text: 'Hour-by-hour energy and variety' }),
          el('p', { class: 'by', text: 'Variety is measured in bits: 0 means one artist or genre; higher values mean a more varied mix. Energy is on a 0–100 scale.' }));
        const hours = el('ul');
        for (const hour of sum.hours) hours.append(el('li', { text: `${clock(hour.from)}: ${hour.works} works; energy ${hour.energy}${hour.energy_target == null ? '' : ` (target ${hour.energy_target})`}; artist variety ${hour.artist_entropy.toFixed(2)}, genre variety ${hour.genre_entropy.toFixed(2)} bits.` }));
        output.append(hours);
      }
      value.works.forEach((row, index) => {
        const item = workNode(row.work, `${index + 1} · ${clock(row.at)}`);
        item.append(el('p', { class: 'by', text: [row.genre, row.mood ? `Mood: ${row.mood}` : null,
          `Energy: ${row.energy}${row.energy_target == null ? '' : ` / target ${row.energy_target}`}`,
          row.explicit ? 'Explicit content' : null].filter(Boolean).join(' · ') }));
        output.append(item);
      });
    }
    control = rehearsalModel({ call, changed: render });
    node.addEventListener('toggle', () => {
      if (node.open) { control.open(); void control.loadMoods(); } else control.close();
    });
    render(control.state);
    return { node, context: control.context, dispose: control.dispose };
  }

  function model({ call, changed = () => {}, now = Date.now }) {
    let epoch = 0, disposed = false;
    const state = { open: false, zones: [], target: '', loading: false, status: null,
      observedAt: null, stale: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && version === epoch;
    const destination = () => state.zones.find((zone) => zone.target === state.target);

    async function readStatus(version) {
      const zone = destination();
      if (!zone) return false;
      const value = statusOf(await call('sessionStatus', { params: { room: zone.target } }), zone);
      if (!current(version)) return false;
      state.status = value;
      state.observedAt = now();
      state.stale = false;
      return true;
    }

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      const version = ++epoch;
      const previous = destination();
      state.loading = true;
      state.stale = true;
      state.message = '';
      publish();
      try {
        const zones = zonesOf(await call('sessionZones'));
        if (!current(version)) return false;
        state.zones = zones;
        if (previous && zones.some((zone) => zone.signature === previous.signature)) {
          return await readStatus(version);
        }
        state.target = '';
        state.status = null;
        state.observedAt = null;
        state.message = previous ? 'The group changed. Choose the group again.'
          : zones.length ? 'Choose a group to inspect its DJ.' : 'No rooms discovered yet.';
        return true;
      } catch (error) {
        if (current(version)) state.message = message(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function select(target) {
      if (disposed || !state.open || !state.zones.some((zone) => zone.target === target)) return false;
      const version = ++epoch;
      state.target = target;
      state.status = null;
      state.observedAt = null;
      state.loading = true;
      state.stale = true;
      state.message = '';
      publish();
      try { return await readStatus(version); }
      catch (error) { if (current(version)) state.message = message(error); return false; }
      finally { if (current(version)) { state.loading = false; publish(); } }
    }

    function close() {
      ++epoch;
      state.open = false;
      state.loading = false;
      state.stale = true;
      publish();
    }
    function dispose() { close(); disposed = true; }
    return { state, refresh, select, close, dispose };
  }

  function panel({ call, el, onedit = null }) {
    let control;
    const node = el('details', { class: 'dj-session' });
    const destination = el('select', { 'aria-label': 'DJ session group',
      onchange: () => control.select(destination.value) });
    const refresh = el('button', { type: 'button', text: 'Refresh DJ', onclick: () => control.refresh() });
    const steerAction = el('button', { type: 'button', text: 'Steer this group', hidden: !onedit, onclick: () => {
      const state = control.state, zone = state.zones.find((z) => z.target === state.target);
      if (zone && state.open && state.status && !state.loading && !state.stale) onedit?.(zone);
    } });
    const status = el('p', { class: 'dj-session-message', role: 'status', 'aria-live': 'polite' });
    const observation = el('div', { class: 'dj-session-observation' });
    const rehearsal = rehearsalPanel({ call, el, workNode });
    const body = el('div', { class: 'dj-session-body' },
      el('p', { text: 'Inspect the DJ’s current and upcoming works, why they were chosen, and what steers the next pick. Refresh for a new observation.' }),
      el('div', { class: 'row' }, destination, refresh, steerAction), status, rehearsal.node, observation);
    node.append(el('summary', { text: 'DJ session · now, next, why & rehearse' }), body);

    function workNode(work, heading) {
      const result = el('article', { class: 'dj-session-work' }, el('h4', { text: heading }),
        el('strong', { text: `${work.composer}: ${work.title}` }),
        el('p', { class: 'by', text: [...(work.performers || []), work.album,
          work.minutes == null ? null : `${work.minutes} min`].filter(Boolean).join(' · ') }));
      if (work.movements > 1) result.append(el('p', {
        text: work.movement ? `Movement ${work.movement} of ${work.movements}${work.movement_title ? `: ${work.movement_title}` : ''}`
          : `${work.movements} movements, kept together in order`,
      }));
      result.append(el('p', { text: `Why: ${work.reason.summary}` }));
      if (work.reason.relaxed?.length) result.append(el('p', { class: 'dj-session-warning',
        text: `Relaxed to find a pick: ${work.reason.relaxed.join('; ')}.` }));
      if (work.reason.factors?.length) {
        const factors = el('ul');
        for (const factor of work.reason.factors) factors.append(el('li', {
          text: `${factor.factor}: ×${(factor.weight / 1000).toLocaleString(undefined, { maximumFractionDigits: 3 })}`,
        }));
        result.append(el('details', {}, el('summary', { text: 'Every selection factor (×1 is neutral)' }), factors));
      }
      return result;
    }

    function render(state) {
      rehearsal.context(state.open && !state.loading && !state.stale
        ? state.zones.find((zone) => zone.target === state.target) : null);
      body.setAttribute('aria-busy', String(state.loading));
      destination.replaceChildren(el('option', { value: '', text: 'Choose a group…', disabled: true }));
      for (const zone of state.zones) destination.append(el('option', { value: zone.target, text: zone.label }));
      destination.value = state.target;
      destination.disabled = state.loading || !state.zones.length;
      refresh.disabled = state.loading;
      steerAction.disabled = state.loading || state.stale || !state.status;
      status.textContent = state.message || (state.loading ? 'Reading the DJ…' : '');
      observation.replaceChildren();
      if (!state.status) return;
      const value = state.status;
      observation.append(el('p', { class: state.stale ? 'dj-session-warning' : 'by',
        text: `${state.stale ? 'Stale observation' : 'Observed'}: ${new Date(state.observedAt).toLocaleString()}. ${value.running ? 'DJ running.' : 'DJ not running.'}` }));
      if (value.problem) observation.append(el('p', { class: 'dj-session-warning', role: 'alert', text: value.problem }));
      if (value.now) observation.append(workNode(value.now, 'Current DJ work'));
      else observation.append(el('p', { text: 'No DJ work currently reported as playing.' }));
      (value.next || []).forEach((work, index) => observation.append(workNode(work, `Up next ${index + 1}`)));
      const steer = value.steering;
      const origin = { session: 'Session override', program: 'Time-of-day program', none: 'No active steering' }[steer.source] || steer.source;
      observation.append(el('h4', { text: 'Steering for the next pick' }),
        el('p', { text: `${origin}${steer.mood ? ` · ${steer.mood}` : ''}` }));
      if (steer.expires_at != null) observation.append(el('p', { text: `Expires ${new Date(steer.expires_at * 1000).toLocaleString()} (this browser’s local time).` }));
      else if (steer.source === 'session') observation.append(el('p', { text: 'Until cleared.' }));
      const filters = constraintsText(steer.constraints);
      if (filters.length) {
        const list = el('ul');
        for (const line of filters) list.append(el('li', { text: line }));
        observation.append(el('details', {}, el('summary', { text: 'Effective filters (mood and session)' }), list));
      }
    }

    control = model({ call, changed: render });
    node.addEventListener('toggle', () => { if (node.open) void control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: () => { control.dispose(); rehearsal.dispose(); }, observeChange: (target) => {
      if (control.state.open && control.state.target === target) return control.refresh();
      return Promise.resolve(false);
    } };
  }

  return { model, panel, routes, zonesOf, statusOf, constraintsText,
    rehearsalRequest, rehearsalTime, rehearsalOf, rehearsalModel, rehearsalPanel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = DjSessionRemote;
else {
  Object.assign(ROUTES, DjSessionRemote.routes);
  const sessionPanel = DjSessionRemote.panel({ call, el, onedit: (zone) =>
    document.dispatchEvent(new CustomEvent('fsonos:dj-edit', { detail: { target: zone.target, signature: zone.signature }, cancelable: true })) });
  document.addEventListener('fsonos:dj-changed', (event) => { void sessionPanel.observeChange(event.detail?.zone); });
  const sessionRoot = el('section', { class: 'scenes', 'aria-label': 'DJ session' }, sessionPanel.node);
  document.getElementById('households')?.before(sessionRoot);
}
