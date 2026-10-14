// Reviewed grouping through the existing policy-governed HTTP actions.
'use strict';

const GroupsRemote = (() => {
  const routes = {
    groupsJoin: ['POST', '/group'],
    groupsLeave: ['POST', '/ungroup'],
    groupsParty: ['POST', '/party'],
  };
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const key = (room, household) => `${room}@${household}`;
  const errorText = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The request failed.';

  function groupsOf(value) {
    if (!Array.isArray(value) || value.length > 256) throw new Error('The group list is unreadable. Refresh rooms.');
    const seen = new Set();
    return Object.freeze(value.map((group) => {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length
          || group.members.length > 256 || !group.members.every(text)
          || !group.members.includes(group.coordinator_room)
          || (group.degraded !== undefined && (!Array.isArray(group.degraded)
            || !group.degraded.every((room) => group.members.includes(room))))) {
        throw new Error('The group list is incomplete. Refresh rooms.');
      }
      for (const room of group.members) {
        const target = key(room, group.household);
        if (seen.has(target)) throw new Error('A room belongs to multiple groups. Refresh rooms.');
        seen.add(target);
      }
      const members = Object.freeze([...group.members].sort());
      const degraded = Object.freeze([...(group.degraded || [])].sort());
      return Object.freeze({ household: group.household, coordinator: group.coordinator_room,
        members, degraded, label: `${members.join(' + ')} (${group.household})`,
        signature: JSON.stringify([group.household, group.coordinator_room, members, degraded]) });
    }));
  }

  function roomsOf(groups) {
    return groups.flatMap((group) => group.members.map((room) => Object.freeze({
      target: key(room, group.household), room, group,
      label: `${room} (${group.household}) — ${group.members.join(' + ')}`,
    })));
  }

  function proposal(groups, source, destination, kind) {
    const rooms = roomsOf(groups);
    const from = rooms.find((room) => room.target === source);
    const to = rooms.find((room) => room.target === destination);
    if (!from) throw new Error('Choose a room first.');
    let route;
    let body;
    let title;
    let affected;
    let household = null;
    if (kind === 'join') {
      if (!to) throw new Error('Choose the group to join.');
      if (from.group.household !== to.group.household) throw new Error('Different households cannot share a group.');
      if (from.group.signature === to.group.signature) throw new Error('These rooms already share a group.');
      route = 'groupsJoin';
      body = { zone: from.target, to: to.target };
      title = `Join ${from.room} to ${to.group.label}`;
      affected = [from.group, to.group];
    } else if (kind === 'leave') {
      if (from.group.members.length === 1) throw new Error('This room already plays on its own.');
      route = 'groupsLeave';
      body = { zone: from.target };
      title = `Separate ${from.room} from ${from.group.label}`;
      affected = [from.group];
    } else if (kind === 'party') {
      household = from.group.household;
      affected = groups.filter((group) => group.household === household);
      if (affected.length < 2) throw new Error('This household already has one group.');
      route = 'groupsParty';
      body = { zone: key(from.group.coordinator, household) };
      title = `Group household ${household} with ${from.group.label}`;
    } else {
      throw new Error('Choose a supported group action.');
    }
    return Object.freeze({ kind, route, body: Object.freeze(body), title, household,
      affected: Object.freeze(affected.map((group) => group.signature).sort()),
      labels: Object.freeze(affected.map((group) => group.label)),
      warning: kind === 'leave'
        ? 'Separating a room can interrupt its playback. The remaining group may elect a new coordinator.'
        : 'Joining replaces the joining rooms\u2019 current audio with the destination group\u2019s music. Moving a coordinator may affect its remaining group.',
    });
  }

  function unchanged(groups, pending) {
    if (pending.household !== null) {
      const current = groups.filter((group) => group.household === pending.household)
        .map((group) => group.signature).sort();
      return JSON.stringify(current) === JSON.stringify(pending.affected);
    }
    return pending.affected.every((signature) => groups.some((group) => group.signature === signature));
  }

  function outcome(value) {
    if (!value || !text(value.done) || typeof value.changed !== 'boolean'
        || (value.notes !== undefined && (!Array.isArray(value.notes)
          || !value.notes.every((note) => note && typeof note.detail === 'string')))) {
      throw new Error('The action returned an unreadable result.');
    }
    return [value.done, ...(value.notes || []).map((note) => note.detail)].join(' ');
  }

  function model({ call, changed = () => {}, updated = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, groups: [], source: '', destination: '', loaded: false,
      loading: false, writing: false, pending: null, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const active = (version) => !disposed && state.open && epoch === version;
    const ready = () => !disposed && state.open && state.loaded && !state.loading && !state.writing;

    async function refresh(keepMessage = false) {
      if (disposed || state.writing) return false;
      state.open = true;
      const version = ++epoch;
      state.loaded = false;
      state.pending = null;
      state.loading = true;
      if (!keepMessage) state.message = '';
      publish();
      try {
        const groups = groupsOf(await call('zones'));
        if (!active(version)) return false;
        state.groups = groups;
        const rooms = roomsOf(groups);
        for (const field of ['source', 'destination']) {
          if (!rooms.some((room) => room.target === state[field])) state[field] = '';
        }
        state.loaded = true;
        if (!groups.length) state.message = 'No rooms discovered yet. Refresh rooms after discovery.';
        return true;
      } catch (error) {
        if (active(version)) { state.groups = []; state.message = errorText(error); }
        return false;
      } finally {
        if (active(version)) { state.loading = false; publish(); }
      }
    }

    function choose(field, value) {
      if (!ready() || !['source', 'destination'].includes(field)
          || !roomsOf(state.groups).some((room) => room.target === value)) return false;
      ++epoch;
      state[field] = value;
      state.pending = null;
      state.message = '';
      publish();
      return true;
    }

    function review(kind) {
      if (!ready()) return false;
      state.pending = null;
      try {
        state.pending = proposal(state.groups, state.source, state.destination, kind);
        state.message = '';
      } catch (error) { state.message = errorText(error); }
      publish();
      return Boolean(state.pending);
    }

    function cancel() {
      if (state.writing) return false;
      ++epoch;
      state.pending = null;
      publish();
      return true;
    }

    async function confirm() {
      if (!ready() || !state.pending) return false;
      const pending = state.pending;
      const version = ++epoch;
      state.pending = null;
      state.writing = true;
      state.message = 'Checking the reviewed groups…';
      publish();
      let attempted = false;
      let success = false;
      try {
        const groups = groupsOf(await call('zones'));
        if (!active(version)) return false;
        if (!unchanged(groups, pending)) throw new Error('The affected groups changed. Review their current membership again.');
        attempted = true;
        state.message = 'Applying the reviewed group action…';
        publish();
        state.message = outcome(await call(pending.route, { body: pending.body }));
        success = true;
      } catch (error) {
        state.message = `${errorText(error)} ${attempted
          ? 'The outcome may be partial. The request was not repeated.' : 'No group change was sent.'} Refresh rooms before another action.`;
      } finally {
        state.loaded = false;
        state.writing = false;
        publish();
      }
      if (!disposed && attempted) {
        try { await updated(); } catch (_) { /* Card refresh cannot replay an action. */ }
      }
      if (success && active(version)) await refresh(true);
      return success;
    }

    function close() {
      state.open = false;
      ++epoch;
      state.pending = null;
      state.loaded = false;
      state.loading = false;
      publish();
    }
    return { state, refresh, choose, review, cancel, confirm, close,
      dispose() { close(); disposed = true; } };
  }

  function panel({ call, el, updated }) {
    const body = el('div', { class: 'groups-body' });
    const node = el('details', { class: 'groups-panel' }, el('summary', { text: 'Rooms & groups' }), body);
    const control = model({ call, updated, changed: render });
    let reviewFocus = null;
    const button = (label, action, disabled) => el('button', {
      type: 'button', text: label, disabled, onclick: action, 'data-group-focus': label,
    });

    function render(state) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-group-focus') : null;
      if (focus && focus.startsWith('Review')) reviewFocus = focus;
      body.replaceChildren();
      const busy = state.loading || state.writing;
      body.setAttribute('aria-busy', String(busy));
      body.append(button('Refresh rooms', () => control.refresh(), busy));
      if (state.message) body.append(el('p', { role: 'status', 'aria-live': 'polite', text: state.message }));
      if (state.loading) body.append(el('p', { text: 'Reading rooms…' }));
      if (!state.loaded) return;
      const groups = el('ul', { class: 'groups-list', 'aria-label': 'Current groups' });
      for (const group of state.groups) groups.append(el('li', { text: `${group.label}${group.degraded.length ? ' — missing bonded player' : ''}` }));
      body.append(groups);
      if (!state.groups.length) return;
      for (const [field, label] of [['source', 'Room to change'], ['destination', 'Destination room / group']]) {
        const select = el('select', { disabled: busy, 'aria-label': label, 'data-group-focus': field,
          onchange: (event) => control.choose(field, event.target.value) });
        select.append(el('option', { value: '', text: 'Choose a room…', disabled: true }));
        for (const room of roomsOf(state.groups)) select.append(el('option', { value: room.target, text: room.label }));
        select.value = state[field];
        body.append(el('label', {}, label, select));
      }
      body.append(el('div', { class: 'row' },
        button('Review join', () => control.review('join'), busy || !state.source || !state.destination),
        button('Review separation', () => control.review('leave'), busy || !state.source),
        button('Review whole household', () => control.review('party'), busy || !state.source)));
      if (state.pending) {
        const pending = state.pending;
        const review = el('section', { class: 'groups-review', 'aria-label': 'Review group change' },
          el('h3', { text: pending.title }), el('p', { text: pending.warning }));
        const affected = el('ul');
        for (const label of pending.labels) affected.append(el('li', { text: label }));
        review.append(el('p', { text: 'Groups affected:' }), affected,
          el('p', { text: 'Room caps apply. Other controllers can still act between the final check and the change. Undo is best-effort through the existing action log.' }),
          el('div', { class: 'row' }, button('Cancel', () => control.cancel(), busy),
            button('Confirm group change', () => control.confirm(), busy)));
        body.append(review);
      }
      const desired = state.pending ? 'Cancel' : focus === 'Cancel' ? reviewFocus : focus;
      const target = [...body.querySelectorAll('[data-group-focus]')]
        .find((element) => element.getAttribute('data-group-focus') === desired);
      if (target && !target.disabled) target.focus({ preventScroll: true });
    }
    node.addEventListener('toggle', () => { if (node.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = GroupsRemote;
else {
  Object.assign(ROUTES, GroupsRemote.routes);
  const root = document.getElementById('room-groups');
  if (root) root.append(GroupsRemote.panel({ call, el, updated: loadZones }).node);
}
