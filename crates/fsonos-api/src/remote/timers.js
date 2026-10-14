// Sleep timers use the daemon's clock, fade engine and speaker backstop.
'use strict';

const TimersRemote = (() => {
  const routes = { sleepRead: ['GET', '/sleep'], sleepWrite: ['POST', '/sleep'] };
  const text = (s) => typeof s === 'string' && s.trim() !== '';
  const errorText = (e) => e && typeof e.message === 'string'
    ? `${e.message}${text(e.hint) ? ` ${e.hint}` : ''}` : 'The request failed.';
  const instant = (s) => text(s) && /(?:Z|[+-]\d\d:\d\d)$/i.test(s) && Number.isFinite(Date.parse(s));
  const displayTime = (s) => instant(s) ? new Date(s).toLocaleString() : 'Not reported';

  // Keep every physical room for room-volume schedules, with its current group.
  function roomsOf(value) {
    if (!Array.isArray(value) || value.length > 512) throw new Error('The room list is unreadable. Refresh.');
    const seen = new Set();
    return value.flatMap((group) => {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length || group.members.length > 512
          || !group.members.every(text) || !group.members.includes(group.coordinator_room)) {
        throw new Error('The group membership is incomplete. Refresh.');
      }
      const signature = JSON.stringify([group.household, group.coordinator_room, [...group.members].sort()]);
      return group.members.map((name) => {
        const target = `${name}@${group.household}`;
        if (seen.has(target)) throw new Error('The room list is ambiguous. Refresh.');
        seen.add(target);
        return Object.freeze({ target, name, signature, household: group.household,
          coordinator: group.coordinator_room, lead: name === group.coordinator_room,
          label: `${group.members.join(' + ')} (${group.household})` });
      });
    });
  }

  function durationOf(value) {
    if (typeof value !== 'string' || value.length > 64) return null;
    const s = value.replace(/\s/g, '').toLowerCase();
    let seconds = 0;
    if (/^\d+$/.test(s)) seconds = Number(s) * 60;
    else {
      if (!/^(?:\d+[hms])+$/.test(s)) return null;
      for (const [, n, unit] of s.matchAll(/(\d+)([hms])/g)) seconds += Number(n) * { h: 3600, m: 60, s: 1 }[unit];
    }
    return Number.isSafeInteger(seconds) && seconds > 0 && seconds <= 23 * 3600 ? s : null;
  }

  function timerOf(value, room) {
    if (!value || value.zone !== room.coordinator || !text(value.done)
        || typeof value.fades !== 'boolean' || typeof value.fading !== 'boolean'
        || typeof value.changed !== 'boolean') throw new Error('The timer response is unreadable. Refresh.');
    if (value.ends == null && value.remaining_secs == null) {
      if (value.fading) throw new Error('The timer response is inconsistent. Refresh.');
      return null;
    }
    if (!instant(value.ends) || !Number.isSafeInteger(value.remaining_secs) || value.remaining_secs < 0
        || (value.fading && !value.fades)) throw new Error('The timer deadline is unreadable. Refresh.');
    return Object.freeze({ ends: value.ends, remaining: value.remaining_secs,
      fades: value.fades, fading: value.fading });
  }

  async function readTimer(call, room) {
    const list = await call('sleepRead', { query: { zone: room.target } });
    if (!Array.isArray(list) || list.length > 1) throw new Error('The timer list is unreadable. Refresh.');
    return list.length ? timerOf(list[0], room) : null;
  }

  function sameTimer(a, b) {
    if (!a || !b) return a === b;
    // A speaker-only timer's deadline is derived from its seconds remaining.
    // Permit rounding jitter, but never a changed daemon deadline or fade phase.
    return a.fades === b.fades && a.fading === b.fading
      && Math.abs(Date.parse(a.ends) - Date.parse(b.ends)) <= (a.fades ? 0 : 2000);
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, rooms: [], target: '', duration: '45m', timer: null,
      loaded: false, loading: false, writing: false, pending: null, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (version) => !disposed && state.open && version === epoch;
    const room = () => state.rooms.find((r) => r.target === state.target);
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; state.timer = null; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const rooms = roomsOf(await call('zones')).filter((r) => r.lead);
        if (!current(version)) return false;
        state.rooms = rooms;
        if (!room()) state.target = '';
        const selected = room();
        if (selected) {
          const timer = await readTimer(call, selected);
          const after = roomsOf(await call('zones')).find((r) => r.target === selected.target);
          if (!current(version)) return false;
          if (!after || after.signature !== selected.signature) throw new Error('The group changed while reading its timer. Refresh.');
          state.timer = timer;
          state.loaded = true;
        }
        return true;
      } catch (e) {
        if (current(version)) { state.loaded = false; state.timer = null; state.message = errorText(e); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function target(value) {
      if (disposed || state.writing || !state.rooms.some((r) => r.target === value)) return false;
      state.target = value;
      return refresh();
    }

    function duration(value) {
      if (disposed || state.writing || typeof value !== 'string') return false;
      state.duration = value;
      state.pending = null;
      publish();
      return true;
    }

    function review(action) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || !room()) return false;
      const selected = room();
      const d = durationOf(state.duration);
      if (!['set', 'extend', 'cancel'].includes(action) || (action !== 'set' && !state.timer)
          || (action !== 'cancel' && !d)) return false;
      const body = action === 'cancel' ? { zone: selected.target, cancel: true }
        : { zone: selected.target, duration: d, ...(action === 'extend' ? { extend: true } : {}) };
      state.pending = Object.freeze({ action, room: selected, timer: state.timer, body: Object.freeze(body) });
      publish();
      return true;
    }

    function cancel() { ++epoch; state.pending = null; publish(); }

    async function confirm() {
      const pending = state.pending;
      if (disposed || !state.open || state.loading || state.writing || !state.loaded || !pending) return false;
      const version = ++epoch;
      state.writing = true;
      state.pending = null;
      publish();
      let sent = false;
      try {
        const before = roomsOf(await call('zones')).find((r) => r.target === pending.room.target);
        if (!current(version)) return false;
        if (!before || before.signature !== pending.room.signature) throw new Error('The group changed. Refresh and review it again.');
        const timer = await readTimer(call, before);
        const after = roomsOf(await call('zones')).find((r) => r.target === before.target);
        if (!current(version)) return false;
        if (!after || after.signature !== before.signature || !sameTimer(pending.timer, timer)) {
          throw new Error('The group or timer changed. Refresh and review it again.');
        }
        sent = true;
        const result = await call('sleepWrite', { body: pending.body });
        const updated = timerOf(result, before);
        if ((pending.action === 'cancel') !== (updated === null)) throw new Error('The reply did not confirm the requested timer change.');
        state.message = result.done;
        return true;
      } catch (e) {
        state.message = `${errorText(e)}${sent ? ' The outcome may be uncertain; the request was not repeated.' : ''} Refresh before another change.`;
        return false;
      } finally {
        state.writing = false;
        state.loaded = false;
        state.timer = null;
        publish();
      }
    }

    return { state, refresh, target, duration, review, cancel, confirm,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  function panel({ call, el }) {
    const content = el('div', { class: 'automation-body' });
    const node = el('details', { class: 'automation scenes' }, el('summary', { text: 'Sleep timers' }), content);
    const control = model({ call, changed: render });
    const button = (label, work, disabled) => el('button', { type: 'button', text: label,
      disabled, 'data-timer-focus': label, onclick: work });
    let reviewFocus = '';
    function render(s) {
      const active = document.activeElement;
      const focus = content.contains(active) ? active.getAttribute('data-timer-focus') : null;
      const caret = active && active.selectionStart;
      if (s.pending && focus && focus !== 'Cancel') reviewFocus = focus;
      content.replaceChildren();
      content.setAttribute('aria-busy', String(s.loading || s.writing));
      const busy = s.loading || s.writing;
      const select = el('select', { 'aria-label': 'Sleep timer group', 'data-timer-focus': 'group', disabled: busy,
        onchange: (e) => control.target(e.target.value) }, el('option', { value: '', text: 'Choose a group…', disabled: true }));
      for (const r of s.rooms) select.append(el('option', { value: r.target, text: r.label }));
      select.value = s.target;
      const input = el('input', { type: 'text', value: s.duration, maxlength: 64,
        'aria-label': 'Sleep timer duration', 'data-timer-focus': 'duration', disabled: busy,
        oninput: (e) => control.duration(e.target.value) });
      content.append(el('p', { text: 'Pause the selected group later. Timers run on the daemon or speaker, not in this browser.' }),
        el('label', {}, 'Group', select), button('Refresh timer', () => control.refresh(), busy));
      if (s.message) content.append(el('p', { role: 'status', text: s.message }));
      if (s.loading) content.append(el('p', { text: 'Reading timer…' }));
      if (s.loaded) {
        content.append(el('p', { text: s.timer
          ? `${s.timer.fading ? 'Fading now. ' : ''}Pauses at ${displayTime(s.timer.ends)} (your browser time). ${s.timer.fades ? 'Daemon fade enabled.' : 'Speaker timer: no fade.'}`
          : 'No active sleep timer for this group.' }));
      }
      content.append(el('label', {}, 'Duration (45m, 1h30m or 90s; maximum 23 hours)', input));
      const ready = s.loaded && !busy && !s.pending;
      content.append(el('div', { class: 'row' },
        button('Set timer', () => control.review('set'), !ready || !durationOf(s.duration)),
        button('Extend timer', () => control.review('extend'), !ready || !s.timer || !durationOf(s.duration)),
        button('Cancel timer', () => control.review('cancel'), !ready || !s.timer)));
      if (s.pending) {
        const p = s.pending;
        content.append(el('section', { class: 'automation-review', 'aria-label': 'Review sleep timer' },
          el('p', { text: `${p.action === 'cancel' ? 'Cancel the timer' : `${p.action === 'extend' ? 'Extend the timer by' : 'Set a timer for'} ${p.body.duration}`} for ${p.room.label}?` }),
          el('p', { text: 'The timer affects the group, not just one room. Setting replaces an existing timer. Extending adds time. Cancelling a fading timer can end its fade.' }),
          el('div', { class: 'row' }, button('Cancel', () => control.cancel(), false), button('Confirm timer change', () => control.confirm(), busy))));
      }
      const key = s.pending ? 'Cancel' : focus === 'Cancel' ? reviewFocus : focus;
      const next = [...content.querySelectorAll('[data-timer-focus]')].find((e) => e.getAttribute('data-timer-focus') === key);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (key === 'duration' && Number.isInteger(caret)) next.setSelectionRange(caret, caret);
      }
    }
    node.addEventListener('toggle', () => { if (node.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes, roomsOf, text, instant, displayTime, errorText, durationOf };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = TimersRemote;
else {
  Object.assign(ROUTES, TimersRemote.routes);
  document.getElementById('scenes-section')?.before(TimersRemote.panel({ call, el }).node);
}
