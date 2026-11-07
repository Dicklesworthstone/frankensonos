// Immediate group-scoped DJ direction through the native steering/start contracts.
'use strict';
const DashboardDirection = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const S = typeof DjSteeringRemote !== 'undefined' ? DjSteeringRemote : require('./dj-steering.js');
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  const playbackIdentity = state => JSON.stringify([state.transport_state, state.track?.uri ?? null, state.track?.queue_position ?? null]);
  function groupOf(value, target, scope) {
    const groups = C.zones(value), group = groups.find(zone => C.key(zone) === target);
    if (!group || C.shape(group) !== scope) fail('This listening group changed. Refresh playback and read its DJ again.');
    return Object.freeze({ ...S.zonesOf([group])[0], zone: group });
  }
  function moodsOf(value) {
    const names = S.moodsOf(value);
    if (new Set(names.map(name => name.trim().toLowerCase())).size !== names.length) fail('The mood names are ambiguous.');
    return Object.freeze(value.moods.map(mood => {
      const constraints = D.constraintsOf(mood.constraints);
      return Object.freeze({ name: mood.name, constraints, signature: JSON.stringify([mood.name, constraints]) });
    }));
  }
  function draftOf(steering) {
    const c = steering.source === 'session' ? steering.added : {}, direction = { ...D.draft() };
    for (const key of Object.keys(D.lists)) {
      if ((c[key] || []).some(value => typeof value === 'string' && /[\r\n]/.test(value))) {
        fail('A saved filter contains a line break and cannot be edited without changing its meaning. Use the native API to replace it.');
      }
      direction[key] = (c[key] || []).join('\n');
    }
    for (const key of ['min_work_minutes', 'max_work_minutes', 'energy_bias']) if (c[key] != null) direction[key] = String(c[key]);
    direction.allow_long_works = c.allow_long_works === true;
    direction.strict = c.strict === true;
    return Object.freeze({ mood: S.sessionMoodOf(steering),
      expiry: steering.source === 'session' && steering.expires_at !== null ? 'keep' : 'until',
      minutes: '', direction: Object.freeze(direction) });
  }
  function describeFilters(value) {
    const constraints = D.constraintsOf(value);
    return Object.entries(constraints).map(([key, item]) => {
      if (D.lists[key]) return `${D.lists[key]}: ${item.join('; ')}`;
      if (key === 'allow_long_works') return 'Long works allowed';
      if (key === 'strict') return 'Strict filters; new picks wait if none match';
      if (key === 'energy_bias') return `Energy shift ${item > 0 ? '+' : ''}${item}`;
      return `${key === 'min_work_minutes' ? 'Minimum' : 'Maximum'} work length ${item} minutes`;
    }).join(' · ') || 'No additional filters';
  }
  const deadline = value => new Date(value * 1000).toLocaleString() + ' (this browser’s time; deadline reported by the house)';

  function model({ call, controller, changed = () => {}, updated = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', label: '', group: null, status: null, moods: [],
      draft: null, dirty: false, stale: true, loading: false, writing: false, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open && controller.state.active;
    const scope = () => controller.selected() ? C.shape(controller.selected().zone) : '';
    const matching = () => state.target === controller.state.selected && state.scope === scope();
    const canRead = () => !!(live() && controller.selected()?.fresh && controller.state.topologyFresh
      && !state.loading && !state.writing && !controller.state.busy && !controller.state.loading);
    const editable = () => !!(live() && matching() && state.draft && !state.loading && !state.writing && !controller.state.busy);
    const ready = () => !!(editable() && state.status && !state.stale && controller.state.topologyFresh
      && now() - readAt < MAX_AGE && controller.canControl());
    const canKeep = () => {
      const s = state.status?.steering;
      return !!(s?.source === 'session' && s.expires_at !== null && Number.isInteger(s.expires_in_secs)
        && s.expires_in_secs - Math.floor(Math.max(0, now() - readAt) / 1000) >= 60 && s.expires_in_secs <= 604800);
    };
    function retire(message, discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null; state.stale = true;
      if (discard) { state.group = null; state.status = null; state.moods = []; state.draft = null; state.dirty = false; state.result = null; readAt = 0; }
      if (message) state.message = message;
    }
    function observe() {
      if (disposed) return;
      if (!matching()) {
        retire('The selected group changed. Read this group’s DJ before editing.', true);
        state.target = controller.state.selected; state.scope = scope();
        state.label = controller.selected() ? `${controller.selected().zone.members.join(' + ')} (${controller.selected().zone.household})` : '';
      } else if (!state.writing && (state.loading || state.pending || !state.stale)
          && (!controller.selected()?.fresh || !controller.state.topologyFresh || !controller.state.active
            || (!state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read the current DJ before reviewing. Your draft is kept; refresh playback first if it is stale.');
      }
      publish();
    }
    async function read({ preserveDraft = state.dirty, keepResult = false } = {}) {
      if (!canRead()) return false;
      const previous = preserveDraft && matching() ? state.draft : null, dirty = previous ? state.dirty : false;
      const receipt = keepResult ? state.result?.done || '' : '';
      retire(); state.target = controller.state.selected; state.scope = scope();
      if (!keepResult) state.result = null;
      const v = version, expected = state.scope, target = state.target, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading the group, its DJ and current mood definitions…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !controller.selected()?.fresh || !controller.state.topologyFresh
            || controller.state.busy || now() - started >= MAX_AGE) fail('The DJ observation is no longer current. Read it again.');
      };
      try {
        const options = { signal: abort.signal };
        const [zones, moods, status] = await Promise.all([call('/zones', undefined, options),
          call('/dj/moods', undefined, options), call(`/zones/${encodeURIComponent(target)}/dj`, undefined, options)]);
        valid();
        const group = groupOf(zones, target, expected), catalog = moodsOf(moods), current = S.statusOf(status, group);
        const draft = previous || draftOf(current.steering);
        state.group = group; state.label = group.label; state.moods = catalog; state.status = current;
        state.draft = draft; state.dirty = dirty; state.stale = false; readAt = started;
        state.message = receipt || (previous ? 'Current DJ state refreshed. Your edited draft is kept; review it against the settings shown above.' : '');
        return true;
      } catch (error) {
        if (live() && v === version) state.message = (receipt ? receipt + ' Current DJ state could not be read. ' : '') + C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function edit(field, value) {
      if (!editable() || !['mood', 'expiry', 'minutes'].includes(field) || typeof value !== 'string'
          || value.length > 128 || (field === 'expiry' && !['keep', 'duration', 'until'].includes(value))) return false;
      ++version; state.pending = null; state.result = null; state.dirty = true;
      state.draft = Object.freeze({ ...state.draft, [field]: value }); state.message = ''; publish(); return true;
    }
    function editDirection(field, value) {
      if (!editable() || field === 'minutes') return false;
      try {
        const direction = D.editDraft(state.draft.direction, field, value);
        ++version; state.pending = null; state.result = null; state.dirty = true;
        state.draft = Object.freeze({ ...state.draft, direction }); state.message = ''; publish(); return true;
      } catch (error) { state.message = C.problem(error); publish(); return false; }
    }
    function proposal(action) {
      if (!ready() || !['save', 'start', 'clear'].includes(action)) fail('Read the current DJ before reviewing this action.');
      if (action === 'start' && state.status.running) fail('The DJ is already running. Save steering for its next picks instead.');
      if (action === 'clear' && !['session', 'unavailable'].includes(state.status.steering.source)) fail('This group is already following its program.');
      const draft = state.draft, clear = action === 'clear';
      if (!clear && draft.mood === null) fail('Choose whether this session follows the time-of-day mood or pins a named mood. Its effective mood alone cannot tell us.');
      if (!clear && draft.expiry === 'keep' && !canKeep()) fail('The current deadline is too close or unavailable. Choose a new duration or until cleared.');
      if (!clear && draft.expiry === 'duration' && !draft.minutes) fail('Enter the new duration in whole minutes.');
      const direction = clear ? null : D.fromDraft({ ...draft.direction, minutes: draft.expiry === 'duration' ? draft.minutes : '' }, draft.mood || null);
      const body = S.requestOf(state.target, clear ? { clear: true } : { mood: draft.mood,
        constraints: direction.constraints, minutes: draft.expiry === 'duration' ? draft.minutes : '', keep_expiry: draft.expiry === 'keep' },
      state.moods.map(mood => mood.name));
      const mood = body.mood ? state.moods.find(item => item.name === body.mood) : null;
      return Object.freeze({ version, action, target: state.target, scope: state.scope, label: state.label, group: state.group,
        body, mood, running: state.status.running, steering: S.signatureOf(state.status.steering),
        playback: action === 'start' ? playbackIdentity(controller.selected().state) : null,
        keepExpiry: !clear && draft.expiry === 'keep', deadline: !clear && draft.expiry === 'keep' ? state.status.steering.expires_at : null });
    }
    function canReview(action) { try { proposal(action); return true; } catch (_) { return false; } }
    function review(action) {
      state.pending = null;
      try { state.pending = proposal(action); state.message = ''; state.result = null; }
      catch (error) { state.message = C.problem(error); }
      publish(); return state.pending;
    }
    const canApply = () => !!(ready() && state.pending && state.pending.version === version
      && (!state.pending.keepExpiry || canKeep()));
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      reading = abort; state.pending = null; state.writing = true; state.result = null; state.message = 'Checking the group and its current steering…'; publish();
      let answer = null, ok = false;
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || target !== reviewed.target || !matching() || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('This direction review is no longer current. Read the DJ and review again.');
          };
          const options = { signal: abort.signal };
          valid();
          groupOf(await call('/zones', undefined, options), target, reviewed.scope); valid();
          if (reviewed.mood) {
            const moods = moodsOf(await call('/dj/moods', undefined, options)); valid();
            if (moods.find(mood => mood.name === reviewed.mood.name)?.signature !== reviewed.mood.signature) {
              fail('The selected mood changed. Read its current definition and review again.');
            }
          }
          if (reviewed.action === 'start') {
            const playback = C.playback(await call(`/zones/${encodeURIComponent(target)}/state`, undefined, options), reviewed.group.zone); valid();
            if (playbackIdentity(playback) !== reviewed.playback) fail('Playback changed while you reviewed starting the DJ. Refresh and review again.');
          }
          const status = S.statusOf(await call(`/zones/${encodeURIComponent(target)}/dj`, undefined, options), reviewed.group); valid();
          if (S.signatureOf(status.steering) !== reviewed.steering || status.running !== reviewed.running) {
            fail('The DJ or its steering changed or expired. Read the current session and review again.');
          }
          let body = reviewed.body;
          if (reviewed.keepExpiry) {
            const remaining = status.steering.expires_in_secs;
            if (!Number.isInteger(remaining) || remaining < 60 || remaining > 604800) fail('The current deadline is too close to preserve. Choose a new duration.');
            body = Object.freeze({ ...body, for_secs: remaining });
          }
          valid(); answer = await send(reviewed.action === 'start' ? '/dj/start' : '/dj/steer', body);
          return answer;
        }, { wholeHouse: false });
        if (!disposed && matching()) {
          if (ok) {
            const done = [answer.done, ...(answer.notes || []).map(note => note.detail)].join(' ');
            state.result = Object.freeze({ action: reviewed.action, target: reviewed.target, done });
            state.message = done; state.dirty = false;
          } else state.message = controller.state.message || 'No direction was saved. Refresh playback and read the DJ again.';
        }
      } finally {
        if (reading === abort) reading = null;
        state.writing = false; retire(); publish();
      }
      if (ok && state.target === reviewed.target && state.scope === reviewed.scope) {
        if (canRead()) await read({ preserveDraft: false, keepResult: true });
        if (live()) { try { await updated(); } catch (_) { /* Native acceptance remains authoritative. */ } }
      }
      return ok;
    }
    return { state, canRead, editable, canKeep, canReview, review, canApply, read, edit, editDirection, apply, observe,
      cancelReview() { if (state.writing) return false; ++version; state.pending = null; publish(); return true; },
      open() {
        if (disposed || !active || state.writing) return Promise.resolve(false);
        state.open = true; observe();
        if (!canRead()) { state.message = 'Refresh playback, then read this group’s DJ before reviewing settings.'; publish(); return Promise.resolve(false); }
        return read();
      },
      close() { state.open = false; retire(undefined, true); publish(); },
      suspend() { active = false; state.open = false; retire(undefined, true); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(undefined, true); },
    };
  }

  function panel({ call, controller, updated, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('direction-dialog'), trigger = $('open-direction');
    let opener = null, shown = null, reviewFocus = null, catalog = null, formKey = '', filtersOpen = false;
    const el = (tag, attrs = {}, ...children) => {
      const node = doc.createElement(tag);
      for (const [key, value] of Object.entries(attrs)) {
        if (key === 'text') node.textContent = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
        else if (['value', 'checked', 'disabled', 'open'].includes(key)) node[key] = value;
        else node.setAttribute(key, value);
      }
      node.append(...children); return node;
    };
    const control = model({ call, controller, updated, now, changed: render });
    function render(state) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) dialog.showModal();
      else if (!state.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('direction-target').textContent = state.label || 'Choose a listening group.';
      $('direction-read').disabled = !control.canRead();
      const status = state.status, steering = status?.steering;
      $('direction-current').textContent = status ? `${status.running ? 'DJ running' : 'DJ stopped'} · ${state.stale ? 'last observation' : 'current observation'}` : 'DJ state not yet read.';
      const unavailable = steering?.source === 'unavailable';
      $('direction-effective').textContent = !steering ? '' : unavailable
        ? 'Steering could not be read; new selections are deferred.'
        : `Effective mood: ${steering.mood || 'none reported'} · ${describeFilters(steering.constraints)}`;
      $('direction-added').textContent = !steering ? '' : unavailable
        ? 'Session additions are unavailable.' : `Session additions: ${describeFilters(steering.added)}`;
      $('direction-deadline').textContent = !steering ? '' : unavailable ? 'Session timing is unavailable.'
        : steering.source !== 'session' ? 'Following the time-of-day program.'
          : steering.expires_at === null ? 'Session lasts until cleared.' : `Session deadline: ${deadline(steering.expires_at)}`;
      $('direction-problem').textContent = status?.problem || '';
      $('direction-form').hidden = !state.draft;
      const disabled = !control.editable();
      for (const field of ['mood', 'expiry', 'minutes']) $('direction-' + field).disabled = disabled;
      if (catalog !== state.moods) {
        catalog = state.moods;
        $('direction-mood').replaceChildren(el('option', { value: '', text: 'Choose how this session handles mood', disabled: true }),
          el('option', { value: 'null', text: 'Follow time-of-day mood' }),
          ...catalog.map(mood => el('option', { value: JSON.stringify(mood.name), text: mood.name })));
      }
      if (state.draft) {
        const d = state.draft, mood = $('direction-mood');
        if (d.mood && !state.moods.some(value => value.name === d.mood)
            && ![...mood.options].some(option => option.value === JSON.stringify(d.mood))) {
          mood.append(el('option', { value: JSON.stringify(d.mood), text: `${d.mood} (not listed; choose another mood)` }));
        }
        mood.value = d.mood === null ? '' : JSON.stringify(d.mood || null);
        $('direction-mood-help').textContent = unavailable
          ? 'Choose new settings or clear the unreadable session to return to the program.'
          : d.mood === null
          ? 'This daemon reports only the effective mood. Choose whether to follow the time-of-day mood or pin a named mood before replacing the session.'
          : d.mood ? 'This named mood stays pinned for the session.' : 'The session follows the house’s time-of-day mood as it changes.';
        for (const field of ['expiry', 'minutes']) {
          const input = $('direction-' + field); if (input.value !== d[field]) input.value = d[field];
        }
        $('direction-keep').disabled = !control.canKeep();
        $('direction-duration-field').hidden = d.expiry !== 'duration';
        $('direction-minutes').disabled = disabled || d.expiry !== 'duration';
        const key = JSON.stringify([d.direction, disabled]);
        if (key !== formKey) {
          formKey = key;
          const focused = doc.activeElement?.getAttribute('data-direction-focus'), caret = doc.activeElement?.selectionStart;
          const node = D.editor({ draft: d.direction, change: control.editDirection, disabled, el, idPrefix: 'live-direction',
            focusAttribute: 'data-direction-focus', open: filtersOpen, toggle: value => { filtersOpen = value; },
            includeDuration: false, note: 'These are this group’s extra session filters. Saving replaces its prior extras; the mood and standing preferences still apply. Already queued works stay queued.' });
          $('direction-filters').replaceChildren(node);
          const input = [...node.querySelectorAll('[data-direction-focus]')].find(value => value.getAttribute('data-direction-focus') === focused);
          if (input && !input.disabled) {
            input.focus({ preventScroll: true });
            if (Number.isInteger(caret) && (input.type === 'text' || input.tagName === 'TEXTAREA')) input.setSelectionRange(caret, caret);
          }
        }
      }
      for (const [id, action] of [['direction-review', 'save'], ['direction-start', 'start'], ['direction-clear', 'clear']]) {
        $(id).disabled = !control.canReview(action);
      }
      const pending = state.pending;
      $('direction-preview').hidden = !pending;
      if (pending) {
        $('direction-preview-title').textContent = pending.action === 'clear' ? 'Return this group to its program'
          : pending.action === 'start' ? 'Start the DJ with this direction' : 'Save this group’s direction';
        $('direction-preview-settings').textContent = pending.action === 'clear' ? 'Clear the group’s temporary mood and filters.'
          : `Mood: ${pending.body.mood || 'time-of-day mood'} · ${describeFilters(pending.body.constraints)}`;
        $('direction-preview-expiry').textContent = pending.action === 'clear' ? '' : pending.keepExpiry
          ? `Keep the current deadline: ${deadline(pending.deadline)}. Confirmation uses the daemon’s freshly reported remaining seconds.`
            : pending.body.for_secs ? `New duration: ${pending.body.for_secs / 60} minutes from saving.` : 'Until cleared.';
        $('direction-preview-effect').textContent = pending.action === 'start'
          ? 'Starts this group’s DJ with these settings. A fresh session can replace the current source and queue. Already queued DJ works remain.'
          : 'Changes future DJ picks. This does not start, skip or restart playback, and already queued works remain. Standing preferences are kept.';
      }
      $('direction-confirm').disabled = !control.canApply();
      $('direction-confirm').textContent = pending?.action === 'start' ? 'Start DJ with settings' : pending?.action === 'clear' ? 'Return to program' : 'Save direction';
      $('direction-back').disabled = state.writing;
      $('direction-message').textContent = state.message;
      dialog.setAttribute('aria-busy', String(state.loading || state.writing));
      if (pending && pending !== shown) { reviewFocus = doc.activeElement; $('direction-preview').focus(); }
      else if (!pending && shown && state.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('close-direction').addEventListener('click', () => control.close());
    $('direction-read').addEventListener('click', () => { void control.read(); });
    $('direction-mood').addEventListener('change', event => {
      if (event.target.value) control.edit('mood', JSON.parse(event.target.value) ?? '');
    });
    for (const field of ['expiry', 'minutes']) $('direction-' + field).addEventListener(field === 'minutes' ? 'input' : 'change',
      event => control.edit(field, event.target.value));
    $('direction-review').addEventListener('click', () => control.review('save'));
    $('direction-start').addEventListener('click', () => control.review('start'));
    $('direction-clear').addEventListener('click', () => control.review('clear'));
    $('direction-confirm').addEventListener('click', () => { void control.apply(); });
    $('direction-back').addEventListener('click', () => control.cancelReview());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { MAX_AGE, groupOf, moodsOf, draftOf, describeFilters, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardDirection;
