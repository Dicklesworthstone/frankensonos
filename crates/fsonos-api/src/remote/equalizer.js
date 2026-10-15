// Per-speaker tone, including members that do not coordinate their group.
'use strict';

const EqualizerRemote = (() => {
  const routes = {
    eqRead: ['GET', '/zones/{room}/eq'],
    eqWrite: ['POST', '/eq'],
  };
  const fields = Object.freeze(['bass', 'treble', 'loudness']);
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const level = (value) => Number.isInteger(value) && value >= -10 && value <= 10;
  const valid = (value) => value && level(value.bass) && level(value.treble)
    && typeof value.loudness === 'boolean';
  const same = (left, right) => valid(left) && valid(right)
    && fields.every((field) => left[field] === right[field]);
  const tone = (value) => ({ bass: value.bass, treble: value.treble, loudness: value.loudness });
  const signed = (value) => value > 0 ? `+${value}` : String(value);
  const problem = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The request failed.';

  function rooms(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable. Refresh rooms.');
    const result = [], seen = new Set();
    for (const group of value) {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length
          || !group.members.every(text) || !group.members.includes(group.coordinator_room)) {
        throw new Error('The room list is incomplete. Refresh rooms.');
      }
      for (const room of group.members) {
        const target = `${room}@${group.household}`;
        if (seen.has(target)) throw new Error('The room list has conflicting entries. Refresh rooms.');
        seen.add(target);
        result.push(Object.freeze({ target, room, label: `${room} (${group.household})` }));
      }
    }
    return result;
  }

  function observed(value, room) {
    if (!value || value.room !== room.room || !valid(value)) {
      throw new Error('The sound response is incomplete or outside the supported range. Read it again.');
    }
    return Object.freeze({ room: value.room, ...tone(value) });
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0, disposed = false;
    const state = { open: false, rooms: [], target: '', current: null, draft: null,
      loading: false, writing: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && epoch === version;
    const selected = () => state.rooms.find((room) => room.target === state.target);
    const invalidate = () => { ++epoch; state.current = null; state.draft = null; state.loading = false; };
    const editable = () => !disposed && state.open && !state.loading && !state.writing
      && state.current && valid(state.draft);

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      const previous = state.target;
      invalidate();
      const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const fresh = rooms(await call('zones'));
        if (!current(version)) return false;
        state.rooms = fresh;
        state.target = fresh.some((room) => room.target === previous) ? previous : '';
        if (!fresh.length) state.message = 'No rooms found. Refresh after speaker discovery.';
        return true;
      } catch (error) {
        if (current(version)) { state.rooms = []; state.target = ''; state.message = problem(error); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function read(target = state.target) {
      if (disposed || !state.open || state.writing) return false;
      const room = state.rooms.find((entry) => entry.target === target);
      if (!room) return false;
      invalidate();
      state.target = target;
      const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const value = observed(await call('eqRead', { params: { room: room.target } }), room);
        if (!current(version)) return false;
        state.current = value; state.draft = tone(value);
        return true;
      } catch (error) {
        if (current(version)) state.message = problem(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    // Range controls edit locally on input; only Apply dispatches a write.
    function choose(field, value, notify = true) {
      if (!editable() || !fields.includes(field)
          || (field === 'loudness' ? typeof value !== 'boolean' : !level(value))) return false;
      state.draft = { ...state.draft, [field]: value };
      state.message = '';
      if (notify) publish();
      return true;
    }

    function flat() {
      if (!editable()) return false;
      state.draft = { bass: 0, treble: 0, loudness: false };
      state.message = ''; publish(); return true;
    }

    function discard() {
      if (!editable()) return false;
      state.draft = tone(state.current); state.message = ''; publish(); return true;
    }

    function canApply() {
      return Boolean(editable() && selected() && !same(state.current, state.draft));
    }

    async function apply() {
      if (!canApply()) return false;
      const room = selected(), before = state.current, wanted = tone(state.draft);
      const body = { zone: room.target };
      // Do not overwrite controls the person did not edit.
      for (const field of fields) if (before[field] !== wanted[field]) body[field] = wanted[field];
      const version = ++epoch;
      state.writing = true; state.message = 'Checking this speaker and its current sound…'; publish();
      let dispatched = false;
      try {
        const fresh = rooms(await call('zones'));
        if (!current(version)) return false;
        if (!fresh.some((entry) => entry.target === room.target)) {
          throw new Error('The selected room is no longer available. Refresh rooms.');
        }
        const now = observed(await call('eqRead', { params: { room: room.target } }), room);
        if (!current(version)) return false;
        if (!same(before, now)) {
          throw new Error('The sound changed since you read it. Read the current sound before editing again.');
        }
        // The API remains the authority. Read-before-write is not a lock
        // against another controller, and a failed multi-part edit may be partial.
        dispatched = true;
        const result = await call('eqWrite', { body });
        const confirmed = observed(result && result.eq, room);
        if (!result || !text(result.done) || !same(confirmed, wanted)) {
          throw new Error('The requested sound was not confirmed by the response.');
        }
        if (current(version)) {
          state.current = confirmed; state.draft = tone(confirmed); state.message = result.done;
        }
        return true;
      } catch (error) {
        if (current(version)) {
          state.current = null; state.draft = null;
          state.message = `${problem(error)} ${dispatched
            ? 'The change may be partial or uncertain. Read current sound before another change; the request was not repeated. The action history may offer undo.'
            : 'No sound change was sent.'}`;
        }
        return false;
      } finally {
        state.writing = false; publish();
      }
    }

    function close() { state.open = false; invalidate(); state.message = ''; publish(); }
    return { state, refresh, read, choose, flat, discard, canApply, apply, close,
      dispose() { disposed = true; state.open = false; invalidate(); } };
  }

  function panel({ call, el }) {
    const body = el('div');
    const details = el('details', { class: 'zone equalizer' }, el('summary', { text: 'Speaker sound' }), body);
    const node = el('section', { class: 'scenes', 'aria-label': 'Speaker sound' }, details);
    const control = model({ call, changed: render });
    const button = (label, action, disabled = false) => el('button', {
      type: 'button', text: label, disabled, 'data-eq-focus': label, onclick: action,
    });
    function render(state) {
      const focus = body.contains(document.activeElement) ? document.activeElement.getAttribute('data-eq-focus') : null;
      const busy = state.loading || state.writing;
      body.replaceChildren(); body.setAttribute('aria-busy', String(busy));
      body.append(el('p', { text: 'Tune one speaker, not its whole group. Changes stay local until you apply them.' }),
        button('Refresh rooms', () => control.refresh(), state.writing));
      const target = el('select', { 'aria-label': 'Speaker to tune', 'data-eq-focus': 'room', disabled: busy });
      target.append(el('option', { value: '', text: 'Choose a room…', disabled: true }));
      for (const room of state.rooms) target.append(el('option', { value: room.target, text: room.label }));
      target.value = state.target;
      target.addEventListener('change', () => control.read(target.value));
      body.append(el('label', {}, 'Speaker ', target), button('Read current sound', () => control.read(), busy || !state.target));
      if (state.loading) body.append(el('p', { text: 'Reading sound…' }));
      const status = el('p', { role: 'status', 'aria-live': 'polite', text: state.message });
      body.append(status);
      if (state.current) {
        body.append(el('p', { text: `Read from speaker: bass ${signed(state.current.bass)}, treble ${signed(state.current.treble)}, loudness ${state.current.loudness ? 'on' : 'off'}.` }));
        const apply = button('Apply sound', () => control.apply(), !control.canApply());
        const discard = button('Discard edits', () => control.discard(), !control.canApply());
        for (const field of ['bass', 'treble']) {
          const title = field === 'bass' ? 'Bass' : 'Treble';
          const output = el('output', { text: signed(state.draft[field]) });
          const input = el('input', { type: 'range', min: -10, max: 10, step: 1,
            value: state.draft[field], disabled: busy, 'aria-label': `${title} for selected speaker`,
            'aria-valuetext': signed(state.draft[field]), 'data-eq-focus': field });
          input.addEventListener('input', () => {
            if (!control.choose(field, Number(input.value), false)) return;
            output.textContent = signed(control.state.draft[field]);
            input.setAttribute('aria-valuetext', output.textContent);
            status.textContent = '';
            apply.disabled = discard.disabled = !control.canApply();
          });
          body.append(el('label', { class: 'eq-level' }, el('span', { text: title }), input, output));
        }
        const loudness = el('input', { type: 'checkbox', checked: state.draft.loudness, disabled: busy,
          'data-eq-focus': 'loudness', onchange: (event) => control.choose('loudness', event.target.checked) });
        body.append(el('label', { class: 'eq-loudness' }, loudness, 'Loudness compensation'),
          el('p', { class: 'eq-help', text: 'Loudness boosts bass and treble at low volume. Bass and treble range from −10 to +10; 0 is flat.' }),
          el('div', { class: 'row' }, apply, discard, button('Prepare flat sound', () => control.flat(), busy)),
          el('p', { class: 'eq-help', text: 'Apply rechecks the room and sound first. Another controller can still change them between the check and the write. Changes use the shared action history and undo.' }));
      }
      const restore = [...body.querySelectorAll('[data-eq-focus]')].find((entry) => entry.getAttribute('data-eq-focus') === focus);
      if (restore && !restore.disabled) restore.focus({ preventScroll: true });
    }
    details.addEventListener('toggle', () => { if (details.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = EqualizerRemote;
else {
  Object.assign(ROUTES, EqualizerRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(EqualizerRemote.panel({ call, el }).node);
}
