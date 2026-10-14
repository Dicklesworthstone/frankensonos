// Persisted routines run on the daemon, never on a browser timer.
'use strict';

const SchedulesRemote = (() => {
  const routes = {
    schedules: ['GET', '/schedules'],
    schedulePause: ['POST', '/schedules/pause'],
    scheduleResume: ['POST', '/schedules/resume'],
    scheduleRemove: ['POST', '/schedules/remove'],
  };
  const text = (v, max = 2048) => typeof v === 'string' && v.trim() !== '' && v.length <= max;
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const instant = (v) => text(v, 64) && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)$/.test(v)
    && Number.isFinite(Date.parse(v));
  const errorText = (e) => text(e?.message) ? `${e.message}${text(e.hint) ? ` ${e.hint}` : ''}` : 'The request failed.';
  const displayTime = (v) => v ? `${new Date(v).toLocaleString()} (your browser time; house reports ${v})` : 'None reported';

  function scheduleOf(value) {
    if (!value || !integer(value.id, 1, Number.MAX_SAFE_INTEGER) || !text(value.when, 256)
        || !text(value.summary) || !text(value.creator, 512) || typeof value.enabled !== 'boolean'
        || (value.next != null && !instant(value.next)) || (value.last_fired != null && !instant(value.last_fired))) {
      throw new Error('The schedule response is unreadable. Refresh before changing it.');
    }
    const fields = {
      dj_start: ['zone', 'mood'], pause: ['zone', 'fade_secs'],
      volume: ['zone', 'volume'], apply_scene: ['scene'],
    };
    if (!Object.hasOwn(fields, value.action)) throw new Error('This schedule action is not supported by this remote.');
    for (const field of ['zone', 'mood', 'scene', 'volume', 'fade_secs']) {
      if (!fields[value.action].includes(field) && value[field] != null) throw new Error('The schedule action has conflicting fields.');
    }
    if (value.action === 'apply_scene' ? !text(value.scene, 512) : !text(value.zone, 512)) {
      throw new Error('The schedule target is missing.');
    }
    if ((value.mood != null && !text(value.mood, 128))
        || (value.action === 'volume' && !integer(value.volume, 0, 100))
        || (value.fade_secs != null && !integer(value.fade_secs, 0, 600))) {
      throw new Error('The schedule settings are unreadable.');
    }
    return Object.freeze({ id: value.id, when: value.when, action: value.action,
      zone: value.zone ?? null, scene: value.scene ?? null, mood: value.mood ?? null,
      volume: value.volume ?? null, fade_secs: value.fade_secs ?? null,
      summary: value.summary, creator: value.creator, enabled: value.enabled,
      next: value.next ?? null, last_fired: value.last_fired ?? null });
  }

  function schedulesOf(value) {
    if (!Array.isArray(value) || value.length > 10000) throw new Error('The schedule list is unreadable.');
    const seen = new Set();
    return value.map((v) => {
      const row = scheduleOf(v);
      if (seen.has(row.id)) throw new Error('The schedule list contains duplicate IDs.');
      seen.add(row.id);
      return row;
    });
  }

  // Next/last runs advance on their own; the definition and enabled state do not.
  function definition(row) {
    return JSON.stringify([row.id, row.when, row.action, row.zone, row.scene,
      row.mood, row.volume, row.fade_secs, row.summary, row.creator]);
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, rows: [], loaded: false, loading: false,
      writing: false, pending: null, message: '' };
    const publish = () => {
      // A view failure must never turn an acknowledged write into a retry.
      if (!disposed) { try { changed(state); } catch (_) { /* The data model remains authoritative. */ } }
    };
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const rows = schedulesOf(await call('schedules'));
        if (!current(version)) return false;
        state.rows = rows;
        state.loaded = true;
        return true;
      } catch (e) {
        if (current(version)) { state.rows = []; state.message = errorText(e); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function review(action, id) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing) return false;
      const row = state.rows.find((r) => r.id === id);
      if (!row || !['pause', 'resume', 'remove'].includes(action)
          || (action === 'pause' && !row.enabled) || (action === 'resume' && row.enabled)) return false;
      state.pending = Object.freeze({ action, row });
      publish();
      return true;
    }

    function cancel() {
      if (disposed || state.writing) return;
      ++epoch;
      state.pending = null;
      publish();
    }

    async function confirm(reviewed) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing
          || !reviewed || reviewed !== state.pending) return false;
      const version = ++epoch;
      state.pending = null;
      state.writing = true;
      state.message = '';
      publish();
      let sent = false;
      try {
        const rows = schedulesOf(await call('schedules'));
        if (!current(version)) return false;
        const row = rows.find((r) => r.id === reviewed.row.id);
        if (!row || definition(row) !== definition(reviewed.row) || row.enabled !== reviewed.row.enabled) {
          throw new Error('The schedule changed or was removed. Refresh and review it again.');
        }
        const route = { pause: 'schedulePause', resume: 'scheduleResume', remove: 'scheduleRemove' }[reviewed.action];
        sent = true;
        const result = scheduleOf(await call(route, { body: { id: row.id } }));
        const expected = reviewed.action === 'remove' ? row.enabled : reviewed.action === 'resume';
        if (definition(result) !== definition(row) || result.enabled !== expected) {
          throw new Error('The reply did not confirm the reviewed schedule change.');
        }
        if (current(version)) {
          state.message = `Schedule #${row.id} ${reviewed.action === 'remove' ? 'removed' : reviewed.action === 'pause' ? 'paused' : 'resumed'}. Refresh to see its current state.`;
        }
        return true;
      } catch (e) {
        if (current(version)) state.message = `${errorText(e)}${sent ? ' The outcome may be uncertain; the request was not repeated.' : ''} Refresh before another change.`;
        return false;
      } finally {
        state.writing = false;
        state.loaded = false;
        publish();
      }
    }

    return { state, refresh, review, cancel, confirm,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  function panel({ call, el }) {
    const content = el('div', { class: 'schedules-body' });
    const node = el('details', { class: 'schedules-remote scenes' }, el('summary', { text: 'Scheduled routines' }), content);
    const control = model({ call, changed: render });
    const button = (label, key, work, disabled) => el('button', { type: 'button', text: label,
      'data-schedule-focus': key, disabled, onclick: work });
    let reviewFocus = null;
    function render(s) {
      const active = document.activeElement;
      const focus = content.contains(active) ? active.getAttribute('data-schedule-focus') : null;
      if (s.pending && focus && focus !== 'cancel') reviewFocus = focus;
      content.replaceChildren();
      content.setAttribute('aria-busy', String(s.loading || s.writing));
      content.append(el('p', { text: 'Routines are stored by the daemon and run with their creator’s rights. Daily and weekly times use the house’s time zone, not this browser’s.' }),
        button('Refresh schedules', 'refresh', () => control.refresh(), s.loading || s.writing));
      if (s.message) content.append(el('p', { role: 'status', text: s.message }));
      if (s.loading) content.append(el('p', { role: 'status', text: 'Reading schedules…' }));
      if (s.writing) content.append(el('p', { role: 'status', text: 'Checking and saving the reviewed change. Do not repeat it from another tab.' }));
      if (s.pending) {
        const p = s.pending;
        content.append(el('section', { class: 'schedule-review', 'aria-label': 'Review schedule change' },
          el('h3', { text: `${p.action === 'remove' ? 'Remove' : p.action === 'pause' ? 'Pause' : 'Resume'} schedule #${p.row.id}?` }),
          el('p', { text: `${p.row.when}: ${p.row.summary}` }),
          el('p', { text: p.action === 'remove' ? 'This permanently removes the saved routine, not the music or scene. A run already in progress is not cancelled. Speaker undo does not restore schedules.'
            : p.action === 'pause' ? 'Prevent future runs; this does not pause music or cancel a run already in progress.'
              : 'Enable future runs. Missed runs are not made up; the daemon decides the next eligible time.' }),
          el('p', { text: 'The saved definition is checked again before sending. A different client can still change it between that check and the write.' }),
          el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel(), false),
            button('Confirm schedule change', 'confirm', () => control.confirm(p), s.writing))));
      }
      if (s.loaded && !s.rows.length) content.append(el('p', { text: 'No saved schedules.' }));
      for (const row of s.rows) {
        const busy = !s.loaded || s.loading || s.writing || !!s.pending;
        content.append(el('article', { class: 'schedule-card' },
          el('h3', { text: `#${row.id} · ${row.enabled ? row.next ? 'Enabled' : 'Enabled; no next run' : 'Paused'}` }),
          el('p', { text: row.when }), el('p', { text: row.summary }),
          el('p', { text: `Creator: ${row.creator}` }),
          el('p', { text: `Next: ${displayTime(row.next)}` }),
          el('p', { text: `Last run or skipped occurrence: ${displayTime(row.last_fired)}` }),
          el('div', { class: 'row' },
            button(row.enabled ? 'Pause routine' : 'Resume routine', `toggle-${row.id}`,
              () => control.review(row.enabled ? 'pause' : 'resume', row.id), busy),
            button('Remove routine', `remove-${row.id}`, () => control.review('remove', row.id), busy))));
      }
      const key = s.pending ? 'cancel' : focus === 'cancel' ? reviewFocus : focus === 'confirm' ? 'refresh' : focus;
      const next = [...content.querySelectorAll('[data-schedule-focus]')].find((e) => e.getAttribute('data-schedule-focus') === key);
      if (next && !next.disabled) next.focus({ preventScroll: true });
    }
    node.addEventListener('toggle', () => { if (node.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, scheduleOf, schedulesOf, definition, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SchedulesRemote;
else {
  Object.assign(ROUTES, SchedulesRemote.routes);
  document.getElementById('scenes-section')?.before(SchedulesRemote.panel({ call, el }).node);
}
