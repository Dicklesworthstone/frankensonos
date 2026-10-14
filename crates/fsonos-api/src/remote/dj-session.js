// Inspect the DJ's actual picks and steering through the public API.
'use strict';

const DjSessionRemote = (() => {
  const routes = {
    sessionZones: ['GET', '/zones'],
    sessionStatus: ['GET', '/zones/{room}/dj'],
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
    const body = el('div', { class: 'dj-session-body' },
      el('p', { text: 'Inspect the DJ’s current and upcoming works, why they were chosen, and what steers the next pick. Refresh for a new observation.' }),
      el('div', { class: 'row' }, destination, refresh, steerAction), status, observation);
    node.append(el('summary', { text: 'DJ session · now, next & why' }), body);

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
    return { node, dispose: control.dispose, observeChange: (target) => {
      if (control.state.open && control.state.target === target) return control.refresh();
      return Promise.resolve(false);
    } };
  }

  return { model, panel, routes, zonesOf, statusOf, constraintsText };
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
