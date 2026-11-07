// Native room grouping shares the dashboard's single command and recovery lane.
'use strict';
const DashboardGroups = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const ACTIONS = Object.freeze({ join: 'Add a room', leave: 'Separate a room', party: 'Group the household' });
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  const address = (room, household) => `${room}@${household}`;
  const signature = zone => JSON.stringify([C.shape(zone), [...zone.degraded].sort()]);
  const describe = zone => `${zone.members.join(' + ')} · ${zone.household}`;
  function inventory(value) {
    const seen = new Set();
    const groups = C.zones(value).map(zone => {
      for (const room of zone.members) {
        const target = address(room, zone.household);
        if (seen.has(target)) fail('Room addresses are ambiguous. Refresh before changing groups.');
        seen.add(target);
      }
      return Object.freeze({ ...zone, members: Object.freeze([...zone.members].sort()) });
    });
    return Object.freeze(groups);
  }
  const householdShape = (groups, household) => JSON.stringify(groups
    .filter(zone => zone.household === household).map(signature).sort());
  function proposal(groups, target, action, room = '') {
    const selected = groups.find(zone => C.key(zone) === target);
    if (!selected || !Object.hasOwn(ACTIONS, action)) fail('Choose a listening group and an action.');
    const label = describe(selected);
    let affected, body, url, title, detail;
    if (action === 'join') {
      const from = groups.find(zone => zone.members.some(name => address(name, zone.household) === room));
      if (!from) fail('Choose the room to add.');
      if (from.household !== selected.household) fail('Rooms in different households cannot be grouped.');
      if (C.key(from) === target) fail('That room already belongs to this group.');
      const name = from.members.find(name => address(name, from.household) === room);
      affected = [selected, from]; url = '/group'; body = { zone: room, to: target };
      title = `Add ${name} to ${label}`;
      detail = `This room leaves its current group and follows ${label}. Its current audio is replaced. ${name === from.coordinator_room && from.members.length > 1 ? 'It currently coordinates its group; the remaining rooms may change coordinator or be interrupted.' : 'Other rooms in its current group remain there.'}`;
    } else if (action === 'leave') {
      const name = selected.members.find(name => address(name, selected.household) === room);
      if (!name) fail('Choose a room in the selected group.');
      if (selected.members.length < 2) fail('This room already plays on its own.');
      affected = [selected]; url = '/ungroup'; body = { zone: room };
      title = `Separate ${name} · ${selected.household}`;
      detail = `This room will play independently. Separating it may interrupt its audio.${name === selected.coordinator_room ? ' It currently coordinates this group; the remaining rooms may change coordinator or be interrupted.' : ' The remaining rooms stay together.'}`;
    } else {
      affected = groups.filter(zone => zone.household === selected.household);
      if (affected.length < 2) fail('This household already has one group.');
      url = '/party'; body = { zone: target };
      title = `Play ${label} throughout ${selected.household}`;
      detail = `All ${affected.reduce((count, zone) => count + zone.members.length, 0)} rooms in this household will follow this group's music. Audio in the other groups is replaced. Other households are unchanged. This is a multi-step operation and may complete only partly.`;
    }
    return Object.freeze({ action, room, target, scope: C.shape(selected), household: selected.household,
      url, body: Object.freeze(body), title, detail,
      affected: Object.freeze(affected), signature: JSON.stringify(affected.map(signature).sort()) });
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', label: '', groups: null, action: 'join', room: '',
      loading: false, writing: false, pending: null, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => controller.selected();
    const live = () => !disposed && active && state.open;
    const matching = () => selected() && state.target === C.key(selected().zone) && state.scope === C.shape(selected().zone);
    const canRead = () => !!(live() && selected()?.fresh && controller.state.active
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.groups && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    function scope() {
      const item = selected();
      state.target = item ? C.key(item.zone) : '';
      state.scope = item ? C.shape(item.zone) : '';
      state.label = item ? describe(item.zone) : 'Choose a listening group';
    }
    function retire(message) {
      ++version; reading?.abort(); reading = null; readAt = 0;
      state.loading = false; state.groups = null; state.room = ''; state.pending = null;
      if (message) state.message = message;
    }
    function affectedFresh(plan) {
      return plan.affected.every(zone => controller.state.items.some(item => item.fresh
        && C.key(item.zone) === C.key(zone) && signature(item.zone) === signature(zone)));
    }
    function observe() {
      if (disposed) return;
      if (!matching()) { retire('The selected group changed. Read its rooms again.'); scope(); }
      else if ((state.groups || state.loading || state.pending) && (!selected()?.fresh
          || !controller.state.active || !controller.state.topologyFresh
          || (state.groups && now() - readAt >= MAX_AGE)
          || (state.pending && !affectedFresh(state.pending)))) {
        retire('The reviewed rooms need a fresh observation. Refresh playback, then read groups again.');
      }
      publish();
    }
    async function read() {
      if (!canRead()) return false;
      retire(); scope();
      const v = version, household = selected().zone.household, abort = new AbortController();
      reading = abort; state.loading = true; state.message = 'Reading the current groups…'; state.result = null; publish();
      try {
        const groups = inventory(await call('/zones', undefined, { signal: abort.signal }));
        if (!live() || v !== version || !matching() || !selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh) return false;
        if (householdShape(groups, household) !== householdShape(inventory(controller.state.items.map(item => item.zone)), household)) {
          fail('The household changed. Refresh playback, then read its groups again.');
        }
        state.groups = groups; readAt = now(); state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function choices() {
      if (!state.groups || !matching() || state.action === 'party') return [];
      const household = selected().zone.household;
      return state.groups.filter(zone => zone.household === household
        && (state.action === 'join' ? C.key(zone) !== state.target : C.key(zone) === state.target))
        .flatMap(zone => zone.members.map(room => Object.freeze({ target: address(room, household),
          label: `${room} · ${household}${room === zone.coordinator_room && zone.members.length > 1 ? ' · group coordinator' : ''}` })));
    }
    function chooseAction(action) {
      if (!editable()) return false;
      const valid = Object.hasOwn(ACTIONS, action);
      ++version; if (valid) state.action = action;
      state.room = ''; state.pending = null; state.result = null;
      state.message = valid ? '' : 'Choose a supported group action.';
      publish(); return valid;
    }
    function chooseRoom(room) {
      if (!editable()) return false;
      const valid = room === '' || choices().some(choice => choice.target === room);
      ++version; state.room = valid ? room : ''; state.pending = null; state.result = null;
      state.message = valid ? '' : 'Choose a room from this household.';
      publish(); return valid;
    }
    function reason() {
      if (!editable()) return 'Read fresh groups before making a change. Use Refresh playback if the dashboard is stale.';
      try {
        const plan = proposal(state.groups, state.target, state.action, state.room);
        return affectedFresh(plan) ? '' : 'Refresh playback so every affected group has a fresh observation.';
      } catch (error) { return C.problem(error); }
    }
    const canReview = () => editable() && !reason();
    function review() {
      if (!canReview()) return null;
      state.pending = Object.freeze({ ...proposal(state.groups, state.target, state.action, state.room), version });
      state.message = ''; state.result = null; publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version && affectedFresh(state.pending));
    async function apply(pending = state.pending) {
      if (!canApply() || pending !== state.pending) return false;
      const v = version;
      let outcome = null;
      state.pending = null; state.writing = true; state.result = null; state.message = 'Checking the affected groups…'; publish();
      try {
        const ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || !matching() || target !== pending.target || !affectedFresh(pending)) {
              fail('The group review is no longer current.');
            }
          };
          valid();
          const fresh = inventory(await call('/zones'));
          valid();
          const next = proposal(fresh, pending.target, pending.action, pending.room);
          if (next.signature !== pending.signature) fail('The affected rooms or coordinators changed. Read groups and review again.');
          const result = await send(pending.url, pending.body);
          if (typeof result?.done !== 'string' || !result.done.trim()) fail('The group change was not confirmed.');
          outcome = Object.freeze({ title: pending.title, action: pending.action });
          return result;
        }, { wholeHouse: true });
        if (ok) { state.result = outcome; state.message = controller.state.message + ' Read groups before making another change.'; }
        else state.message = controller.state.message || 'No group change was sent. Refresh playback before trying again.';
        return ok;
      } finally {
        state.writing = false; retire(); publish();
      }
    }
    return { state, read, choices, chooseAction, chooseRoom, reason, canRead, editable, canReview, review, canApply, apply, observe,
      cancel() { if (state.writing) return false; ++version; state.pending = null; publish(); return true; },
      open() { if (disposed || !active) return Promise.resolve(false); state.open = true; scope(); publish(); return read(); },
      close() { state.open = false; retire(); publish(); },
      suspend() { active = false; state.open = false; retire(); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(); },
    };
  }

  function panel({ call, controller, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('groups-dialog'), trigger = $('open-groups');
    let opener = null, optionsKey = '', shown = null;
    const control = model({ call, controller, now, changed: render });
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('groups-target').textContent = s.label;
      $('groups-read').disabled = !control.canRead();
      $('groups-action').disabled = !control.editable(); $('groups-action').value = s.action;
      const room = $('groups-room'), choices = control.choices(), key = JSON.stringify(choices);
      if (key !== optionsKey || !room.options.length) {
        optionsKey = key;
        const placeholder = doc.createElement('option'); placeholder.value = ''; placeholder.textContent = 'Choose a room';
        room.replaceChildren(placeholder);
        for (const choice of choices) {
          const option = doc.createElement('option'); option.value = choice.target; option.textContent = choice.label; room.append(option);
        }
      }
      room.value = s.room; room.disabled = !control.editable() || !choices.length;
      $('groups-room-field').hidden = s.action === 'party';
      $('groups-note').textContent = control.reason() || (s.action === 'join'
        ? 'The added room follows the selected group’s music.' : s.action === 'leave'
          ? 'The separated room becomes an independent group.' : 'All rooms in this household follow the selected group’s music.');
      $('groups-review').disabled = !control.canReview() || !!s.pending;
      const preview = $('groups-preview'); preview.hidden = !s.pending;
      if (shown !== s.pending) {
        shown = s.pending;
        $('groups-affected').replaceChildren();
        $('groups-preview-title').textContent = shown?.title || '';
        $('groups-preview-detail').textContent = shown?.detail || '';
        for (const zone of shown?.affected || []) {
          const row = doc.createElement('li'); row.textContent = describe(zone)
            + (zone.degraded.length ? ` · unavailable: ${zone.degraded.join(', ')}` : '');
          $('groups-affected').append(row);
        }
        if (shown) preview.focus({ preventScroll: true });
      }
      $('groups-confirm').disabled = !control.canApply();
      $('groups-confirm').textContent = s.pending ? ACTIONS[s.pending.action] : 'Confirm group change';
      $('groups-cancel').disabled = s.writing;
      $('groups-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('groups-read').addEventListener('click', () => { void control.read(); });
    $('groups-action').addEventListener('change', event => control.chooseAction(event.target.value));
    $('groups-room').addEventListener('change', event => control.chooseRoom(event.target.value));
    $('groups-review').addEventListener('click', () => control.review());
    $('groups-confirm').addEventListener('click', () => { void control.apply(); });
    $('groups-cancel').addEventListener('click', () => control.cancel());
    $('close-groups').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { ACTIONS, MAX_AGE, inventory, proposal, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardGroups;
