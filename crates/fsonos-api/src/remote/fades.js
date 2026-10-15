// Reviewed native fades. The daemon, not browser timers, changes the speakers.
'use strict';

const FadesRemote = (() => {
  const routes = {
    fadeState: ['GET', '/zones/{room}/state'],
    fadeVolume: ['POST', '/volume/fade'],
    fadePause: ['POST', '/pause/fade'],
  };
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const volume = (value) => Number.isInteger(value) && value >= 0 && value <= 100;
  const integer = (value, min, max) => typeof value === 'string' && /^[0-9]{1,3}$/.test(value)
    && Number(value) >= min && Number(value) <= max ? Number(value) : null;
  const problem = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The request failed.';

  function groupOf(value) {
    if (!value || !text(value.household) || !text(value.coordinator_room)
        || !Array.isArray(value.members) || !value.members.length
        || !value.members.every(text) || !value.members.includes(value.coordinator_room)
        || new Set(value.members).size !== value.members.length) {
      throw new Error('The group description is incomplete. Refresh rooms and groups.');
    }
    const members = Object.freeze([...value.members]);
    return Object.freeze({ household: value.household, coordinator: value.coordinator_room, members,
      signature: JSON.stringify([value.household, value.coordinator_room, [...members].sort()]) });
  }

  function catalog(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable. Refresh rooms and groups.');
    const seen = new Set();
    return value.map((entry) => {
      const group = groupOf(entry);
      for (const room of group.members) {
        const target = `${room}@${group.household}`;
        if (seen.has(target)) throw new Error('The room list contains conflicting entries. Refresh it.');
        seen.add(target);
      }
      return group;
    });
  }

  function choices(groups, mode) {
    return groups.flatMap((group) => (mode === 'pause' ? [group.coordinator] : group.members)
      .map((room) => Object.freeze({ room, target: `${room}@${group.household}`, group,
        label: `${mode === 'pause' ? group.members.join(' + ') : room} (${group.household})` })));
  }

  function observed(value, target) {
    if (!value || groupOf(value.zone).signature !== target.group.signature
        || !volume(value.volume) || typeof value.mute !== 'boolean'
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)) {
      throw new Error('The speaker state is incomplete or the group changed. Refresh and read again.');
    }
    const track = value.track;
    const uri = track && text(track.uri) && track.uri !== 'NOT_IMPLEMENTED' ? track.uri : null;
    const slot = track?.queue_position ?? null;
    if (slot !== null && (!Number.isInteger(slot) || slot < 1 || slot > 4294967295)) {
      throw new Error('The current queue position is unreadable. Read again.');
    }
    return Object.freeze({ group: target.group.signature, volume: value.volume, mute: value.mute,
      transport: value.transport_state, uri, slot,
      title: track && text(track.title) ? track.title : 'Current source' });
  }

  function outcome(value) {
    if (!value || !text(value.done) || typeof value.changed !== 'boolean'
        || (value.volume != null && !volume(value.volume))
        || (value.notes !== undefined && (!Array.isArray(value.notes)
          || !value.notes.every((note) => note && text(note.detail))))) {
      throw new Error('The fade returned an unreadable acknowledgement.');
    }
    return [value.done, ...(value.notes || []).map((note) => note.detail)].join(' ');
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0, disposed = false;
    const state = { open: false, groups: [], mode: 'volume', target: '', current: null,
      level: '20', seconds: '5', pending: null, loading: false, writing: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && version === epoch;
    const targets = () => choices(state.groups, state.mode);
    const selected = () => targets().find((entry) => entry.target === state.target);
    const invalidate = () => { ++epoch; state.current = null; state.pending = null; state.loading = false; };
    const ready = () => !disposed && state.open && !state.loading && !state.writing;

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      invalidate(); const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const fresh = catalog(await call('zones'));
        if (!current(version)) return false;
        state.groups = fresh;
        if (!selected()) state.target = '';
        if (!fresh.length) state.message = 'No rooms found. Refresh after speaker discovery.';
        return true;
      } catch (error) {
        if (current(version)) { state.groups = []; state.target = ''; state.message = problem(error); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function mode(value) {
      if (!ready() || !['volume', 'pause'].includes(value)) return false;
      if (state.mode === value) return true;
      invalidate(); state.mode = value; state.target = ''; state.message = ''; publish(); return true;
    }

    async function read(target = state.target) {
      if (disposed || !state.open || state.writing) return false;
      const entry = targets().find((each) => each.target === target);
      if (!entry) return false;
      invalidate(); state.target = target; const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const value = observed(await call('fadeState', { params: { room: entry.target } }), entry);
        if (!current(version)) return false;
        state.current = value;
        if (state.mode === 'volume') state.level = String(value.volume);
        return true;
      } catch (error) {
        if (current(version)) state.message = problem(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function edit(field, value, notify = true) {
      if (!ready() || !['level', 'seconds'].includes(field) || typeof value !== 'string') return false;
      // Preserve incomplete input as a draft, but never coerce it into a command.
      state[field] = value; state.pending = null; state.message = '';
      if (notify) publish();
      return true;
    }

    function canReview() {
      if (!ready() || !['volume', 'pause'].includes(state.mode) || !selected() || !state.current || state.pending
          || integer(state.seconds, 1, 60) === null) return false;
      return state.mode === 'volume'
        ? integer(state.level, 0, 100) !== null && Number(state.level) !== state.current.volume
        : state.current.transport === 'playing' && state.current.uri !== null;
    }

    function review() {
      if (!canReview()) return false;
      const entry = selected();
      const body = { zone: entry.target, fade_secs: Number(state.seconds) };
      if (state.mode === 'volume') body.volume = Number(state.level);
      state.pending = Object.freeze({ mode: state.mode, entry, before: state.current,
        body: Object.freeze(body) });
      state.message = ''; publish(); return true;
    }

    function cancel() { state.pending = null; publish(); }

    async function confirm(pending = state.pending) {
      if (!ready() || !pending || pending !== state.pending) return false;
      const version = ++epoch;
      // Consume the exact reviewed intent before any await. Detached buttons
      // cannot confirm a later review or dispatch this one twice.
      state.pending = null; state.writing = true;
      state.message = 'Checking the reviewed speaker, group and current state…'; publish();
      let dispatched = false;
      try {
        const fresh = catalog(await call('zones'));
        if (!current(version)) return false;
        const entry = choices(fresh, pending.mode).find((each) => each.target === pending.entry.target);
        if (!entry || (pending.mode === 'pause' && entry.group.signature !== pending.entry.group.signature)) {
          throw new Error('The reviewed speaker or group changed. Refresh and review again.');
        }
        const now = observed(await call('fadeState', { params: { room: entry.target } }), entry);
        if (!current(version)) return false;
        const before = pending.before;
        if (now.volume !== before.volume || now.mute !== before.mute
            || (pending.mode === 'pause' && (now.transport !== 'playing'
              || now.uri !== before.uri || now.slot !== before.slot))) {
          throw new Error('The reviewed volume, mute or playback state changed. Read and review again.');
        }
        state.message = `Fading for up to ${pending.body.fade_secs} seconds. Closing this panel does not cancel the daemon's action.`;
        publish();
        if (!current(version)) return false;
        dispatched = true;
        const result = await call(pending.mode === 'volume' ? 'fadeVolume' : 'fadePause', { body: pending.body });
        const message = outcome(result);
        if (current(version)) {
          state.current = null;
          state.message = `${message} Read current state before another fade.`;
        }
        return true;
      } catch (error) {
        if (current(version)) {
          state.current = null;
          state.message = `${problem(error)} ${dispatched
            ? 'The fade may have partially completed or may still be running. Read current state and check action history; the request was not repeated.'
            : 'No fade was sent.'}`;
        }
        return false;
      } finally {
        state.writing = false; publish();
      }
    }

    function close() { state.open = false; invalidate(); state.message = ''; publish(); }
    return { state, targets, refresh, mode, read, edit, canReview, review, cancel, confirm, close,
      dispose() { disposed = true; state.open = false; invalidate(); } };
  }

  function panel({ call, el }) {
    const body = el('div');
    const details = el('details', { class: 'zone fades' }, el('summary', { text: 'Volume fades & gentle pause' }), body);
    const node = el('section', { class: 'scenes', 'aria-label': 'Volume fades and gentle pause' }, details);
    const control = model({ call, changed: render });
    const button = (label, action, disabled = false) => el('button', {
      type: 'button', text: label, disabled, 'data-fade-focus': label, onclick: action,
    });
    function render(state) {
      const focus = body.contains(document.activeElement) ? document.activeElement.getAttribute('data-fade-focus') : null;
      const busy = state.loading || state.writing;
      body.replaceChildren(); body.setAttribute('aria-busy', String(busy));
      body.append(el('p', { text: 'Use a native 1–60 second fade now, without creating a timer or schedule.' }),
        button('Refresh rooms and groups', () => control.refresh(), state.writing));
      const mode = el('select', { 'aria-label': 'Fade action', 'data-fade-focus': 'mode', disabled: busy });
      mode.append(el('option', { value: 'volume', text: 'Fade one speaker to a volume' }),
        el('option', { value: 'pause', text: 'Fade out and pause a whole group' }));
      mode.value = state.mode; mode.addEventListener('change', () => control.mode(mode.value));
      const target = el('select', { 'aria-label': state.mode === 'volume' ? 'Speaker to fade' : 'Group to pause',
        'data-fade-focus': 'target', disabled: busy });
      target.append(el('option', { value: '', text: 'Choose a target…', disabled: true }));
      for (const entry of control.targets()) target.append(el('option', { value: entry.target, text: entry.label }));
      target.value = state.target; target.addEventListener('change', () => control.read(target.value));
      body.append(el('label', {}, 'Action ', mode), el('label', {}, 'Target ', target),
        button('Read current state', () => control.read(), busy || !state.target));
      const status = el('p', { role: 'status', 'aria-live': 'polite', text: state.message });
      body.append(status);
      if (state.loading) body.append(el('p', { text: 'Reading current state…' }));
      if (state.current) {
        const now = state.current;
        body.append(el('p', { text: `Read from speaker: volume ${now.volume}, ${now.mute ? 'muted' : 'not muted'}, ${now.transport}.` }));
        if (state.mode === 'pause') {
          body.append(el('p', { text: `Source: ${now.title}.` }));
          if (now.transport !== 'playing' || now.uri === null) {
            body.append(el('p', { text: 'A gentle pause needs a known, playing source. It never resumes paused music.' }));
          }
        }
        const review = button('Review fade', () => control.review(), !control.canReview());
        const seconds = el('input', { type: 'text', inputmode: 'numeric', pattern: '[0-9]*', maxlength: 2,
          value: state.seconds, 'aria-label': 'Fade duration in seconds', 'data-fade-focus': 'seconds', disabled: busy });
        seconds.addEventListener('input', () => {
          const hadReview = Boolean(control.state.pending);
          if (!control.edit('seconds', seconds.value, false)) return;
          if (hadReview) { render(control.state); return; }
          status.textContent = ''; review.disabled = !control.canReview();
        });
        body.append(el('label', {}, 'Duration (1–60 seconds) ', seconds));
        if (state.mode === 'volume') {
          const output = el('output', { text: state.level });
          const level = el('input', { type: 'range', min: 0, max: 100, step: 1, value: state.level,
            'aria-label': 'Target speaker volume', 'data-fade-focus': 'level', disabled: busy });
          level.addEventListener('input', () => {
            const hadReview = Boolean(control.state.pending);
            if (!control.edit('level', level.value, false)) return;
            if (hadReview) { render(control.state); return; }
            output.textContent = level.value; status.textContent = ''; review.disabled = !control.canReview();
          });
          body.append(el('label', { class: 'fade-level' }, 'Target volume ', level, output),
            el('p', { class: 'fade-help', text: 'Only this speaker changes. It stays muted if already muted. The daemon applies house caps and step limits; another volume action may supersede a running fade.' }));
        }
        body.append(review);
      }
      if (state.pending) {
        const pending = state.pending;
        const description = pending.mode === 'volume'
          ? `${pending.entry.label}: volume ${pending.before.volume} → ${pending.body.volume} over ${pending.body.fade_secs} seconds. Other speakers are unchanged.`
          : `${pending.entry.label}: fade out over ${pending.body.fade_secs} seconds, pause the group, then restore its pre-fade volume levels. Playback stays paused.`;
        body.append(el('div', { class: 'fade-review', role: 'group', 'aria-label': 'Review fade' },
          el('h3', { text: 'Review this fade' }), el('p', { text: description }),
          el('p', { text: 'The daemon executes the action under its own policy. Browser preflight is not an atomic lock. A failed fade or restoration may leave a partial result; no automatic retry is made.' }),
          el('div', { class: 'row' }, button('Cancel fade', () => control.cancel()),
            button('Confirm fade', () => control.confirm(pending)))));
      }
      const key = state.pending ? 'Cancel fade' : focus;
      const restore = [...body.querySelectorAll('[data-fade-focus]')].find((entry) => entry.getAttribute('data-fade-focus') === key);
      if (restore && !restore.disabled) restore.focus({ preventScroll: true });
    }
    details.addEventListener('toggle', () => { if (details.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = FadesRemote;
else {
  Object.assign(ROUTES, FadesRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(FadesRemote.panel({ call, el }).node);
}
