// Shuffle and repeat through the same public API as the room cards.
'use strict';

const PlayModeRemote = (() => {
  const routes = {
    modeRead: ['GET', '/zones/{room}/play-mode'],
    modeWrite: ['POST', '/play-mode'],
  };
  const modes = Object.freeze({
    NORMAL: 'In order, no repeat',
    REPEAT_ALL: 'In order, repeat queue',
    REPEAT_ONE: 'Repeat current track',
    SHUFFLE_NOREPEAT: 'Shuffle, no repeat',
    SHUFFLE: 'Shuffle, repeat queue',
    SHUFFLE_REPEAT_ONE: 'Shuffle, repeat current track',
  });
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const known = (value) => typeof value === 'string' && Object.hasOwn(modes, value);
  const problem = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The request failed.';

  function groups(value) {
    if (!Array.isArray(value)) throw new Error('The group list is unreadable. Refresh groups.');
    const seen = new Set();
    return value.map((group) => {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length || !group.members.every(text)
          || !group.members.includes(group.coordinator_room)
          || new Set(group.members).size !== group.members.length) {
        throw new Error('The group list is incomplete. Refresh groups.');
      }
      for (const room of group.members) {
        const key = JSON.stringify([group.household, room]);
        if (seen.has(key)) throw new Error('A room belongs to conflicting groups. Refresh groups.');
        seen.add(key);
      }
      return Object.freeze({ target: `${group.coordinator_room}@${group.household}`,
        room: group.coordinator_room, label: `${group.members.join(' + ')} (${group.household})`,
        signature: JSON.stringify([group.household, group.coordinator_room, [...group.members].sort()]) });
    });
  }

  function observed(value, group) {
    if (!value || value.zone !== group.room || !known(value.mode)
        || typeof value.queue_active !== 'boolean' || typeof value.dj_active !== 'boolean') {
      throw new Error('The play-mode response is incomplete or unsupported. Read it again.');
    }
    return Object.freeze({ zone: value.zone, mode: value.mode,
      queue_active: value.queue_active, dj_active: value.dj_active });
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, groups: [], target: '', current: null, desired: '',
      loading: false, writing: false, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && epoch === version;
    const selected = () => state.groups.find((group) => group.target === state.target);
    const invalidate = () => { ++epoch; state.current = null; state.desired = ''; state.loading = false; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      const previous = selected();
      invalidate();
      const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const fresh = groups(await call('zones'));
        if (!current(version)) return false;
        state.groups = fresh;
        if (!previous || !fresh.some((group) => group.signature === previous.signature)) state.target = '';
        if (!fresh.length) state.message = 'No groups found. Refresh after speaker discovery.';
        return true;
      } catch (error) {
        if (current(version)) { state.groups = []; state.target = ''; state.message = problem(error); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function read(target = state.target) {
      if (disposed || !state.open || state.writing) return false;
      const group = state.groups.find((entry) => entry.target === target);
      if (!group) return false;
      invalidate();
      state.target = target;
      const version = epoch;
      state.loading = true; state.message = ''; publish();
      try {
        const value = observed(await call('modeRead', { params: { room: group.target } }), group);
        if (!current(version)) return false;
        state.current = value; state.desired = value.mode;
        return true;
      } catch (error) {
        if (current(version)) state.message = problem(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function choose(mode) {
      if (disposed || !state.open || state.loading || state.writing || !state.current || !known(mode)) return false;
      if (!state.current.queue_active || (state.current.dj_active && mode !== 'NORMAL')) return false;
      state.desired = mode; state.message = ''; publish();
      return true;
    }

    function canApply() {
      return Boolean(!disposed && state.open && !state.loading && !state.writing && state.current
        && state.current.queue_active && known(state.desired) && state.current.mode !== state.desired
        && (!state.current.dj_active || state.desired === 'NORMAL'));
    }

    async function apply() {
      if (!canApply()) return false;
      const group = selected(), before = state.current, wanted = state.desired;
      if (!group) return false;
      const version = ++epoch;
      state.writing = true; state.message = 'Checking the current group and playback order…'; publish();
      let dispatched = false;
      try {
        const fresh = groups(await call('zones'));
        if (!current(version)) return false;
        if (!fresh.some((entry) => entry.signature === group.signature)) {
          throw new Error('The group changed. Refresh groups before choosing its playback order.');
        }
        const now = observed(await call('modeRead', { params: { room: group.target } }), group);
        if (!current(version)) return false;
        if (now.mode !== before.mode || now.queue_active !== before.queue_active || now.dj_active !== before.dj_active) {
          throw new Error('The source, DJ or playback order changed. Read the current setting again.');
        }
        dispatched = true;
        const result = await call('modeWrite', { body: {
          zone: group.target, mode: wanted, expected_mode: before.mode,
        } });
        const confirmed = observed(result && result.state, group);
        if (!result || !text(result.done) || typeof result.changed !== 'boolean'
            || typeof result.recovered !== 'boolean' || confirmed.mode !== wanted
            || !confirmed.queue_active || (confirmed.dj_active && wanted !== 'NORMAL')) {
          throw new Error('The setting was not confirmed by the response.');
        }
        if (current(version)) { state.current = confirmed; state.desired = confirmed.mode; state.message = result.done; }
        return true;
      } catch (error) {
        if (current(version)) {
          state.current = null; state.desired = '';
          state.message = `${problem(error)} ${dispatched
            ? 'The outcome may be uncertain. Read the setting before another change; the request was not repeated.'
            : 'No mode change was sent.'}`;
        }
        return false;
      } finally {
        state.writing = false; publish();
      }
    }

    function close() { state.open = false; invalidate(); publish(); }
    return { state, refresh, read, choose, canApply, apply, close,
      dispose() { disposed = true; state.open = false; invalidate(); } };
  }

  function panel({ call, el }) {
    const body = el('div');
    const details = el('details', { class: 'zone play-mode' }, el('summary', { text: 'Shuffle & repeat' }), body);
    const node = el('section', { class: 'scenes', 'aria-label': 'Playback order' }, details);
    const control = model({ call, changed: render });
    const button = (label, action, disabled = false) => el('button', {
      type: 'button', text: label, disabled, 'data-mode-focus': label, onclick: action,
    });
    function render(state) {
      const focus = body.contains(document.activeElement) ? document.activeElement.getAttribute('data-mode-focus') : null;
      const busy = state.loading || state.writing;
      body.replaceChildren(); body.setAttribute('aria-busy', String(busy));
      body.append(el('p', { text: 'Change queue playback order without restarting the music.' }),
        button('Refresh groups', () => control.refresh(), state.writing));
      const target = el('select', { 'aria-label': 'Group for playback order', 'data-mode-focus': 'group', disabled: busy });
      target.append(el('option', { value: '', text: 'Choose a group…', disabled: true }));
      for (const group of state.groups) target.append(el('option', { value: group.target, text: group.label }));
      target.value = state.target;
      target.addEventListener('change', () => control.read(target.value));
      body.append(el('label', {}, 'Group ', target), button('Read current setting', () => control.read(), busy || !state.target));
      if (state.loading) body.append(el('p', { text: 'Reading playback order…' }));
      if (state.message) body.append(el('p', { role: 'status', 'aria-live': 'polite', text: state.message }));
      const value = state.current;
      if (value) {
        body.append(el('p', { text: `Current setting: ${modes[value.mode]}.` }));
        if (!value.queue_active) body.append(el('p', { text: 'Another source is selected. Start an existing queue entry first; this control will not switch sources.' }));
        if (value.dj_active) body.append(el('p', { text: 'The active DJ needs sequential order. Stop the DJ before selecting shuffle or repeat.' }));
        const choice = el('select', { 'aria-label': 'Shuffle and repeat setting', 'data-mode-focus': 'mode', disabled: busy || !value.queue_active });
        for (const [mode, label] of Object.entries(modes)) {
          choice.append(el('option', { value: mode, text: label, disabled: value.dj_active && mode !== 'NORMAL' }));
        }
        choice.value = state.desired;
        choice.addEventListener('change', () => control.choose(choice.value));
        body.append(el('label', {}, 'Playback order ', choice), button('Apply playback order', () => control.apply(), !control.canApply()));
      }
      const restore = [...body.querySelectorAll('[data-mode-focus]')].find((entry) => entry.getAttribute('data-mode-focus') === focus);
      if (restore && !restore.disabled) restore.focus({ preventScroll: true });
    }
    details.addEventListener('toggle', () => { if (details.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes, modes };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = PlayModeRemote;
else {
  Object.assign(ROUTES, PlayModeRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(PlayModeRemote.panel({ call, el }).node);
}
