// Persist native routines; the daemon owns timing, permissions and execution.
'use strict';
const DashboardRoutines = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const S = typeof SchedulesRemote !== 'undefined' ? SchedulesRemote : require('./schedules.js');
  const D = typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const MAX_AGE = 60000;
  const fail = message => { throw new Error(message); };
  const effect = action => action === 'remove'
    ? 'Remove this stored routine. A run already in progress is not stopped.'
    : action === 'pause' ? 'Pause future runs. A run already in progress is not stopped.'
      : 'Resume future runs. Missed occurrences are not replayed.';
  const done = (action, row) => action === 'create'
    ? `Created routine #${row.id}. Next run reported by the house: ${row.next}.`
    : `Routine #${row.id} ${action === 'pause' ? 'paused' : action === 'resume' ? 'resumed' : 'removed'}.`;

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0, choicesAt = 0;
    const state = { open: false, rows: [], loaded: false, loading: false, writing: false,
      draft: null, options: null, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open && controller.state.active;
    const canRead = () => live() && !state.loading && !state.writing && !controller.state.busy;
    const editable = () => live() && state.loaded && !state.loading && !state.writing
      && now() - readAt < MAX_AGE && controller.canManage();
    const route = (name, options) => call(S.routes[name][1], undefined, options);
    function retire(discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null;
      if (discard) { state.loaded = false; state.draft = null; state.options = null; state.result = null; }
    }
    function observe() {
      if (!state.writing && state.loaded && now() - readAt >= MAX_AGE) {
        retire(true); state.message = 'Refresh routines before another change.';
      } else if (!state.writing && state.pending && (!controller.canManage() || !live())) {
        state.pending = null;
      }
      publish();
    }
    async function read(explicit = true) {
      if (!canRead()) return false;
      retire(true);
      const v = version, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading saved routines…'; publish();
      const valid = () => {
        if (!live() || v !== version || now() - started >= MAX_AGE) fail('The routine list is no longer current. Refresh it again.');
      };
      try {
        const rows = await controller.refreshStored('schedules', value => { valid(); return S.schedulesOf(value); },
          { explicit, signal: abort.signal });
        valid();
        if (rows === null) fail('The dashboard changed while reading routines. Refresh them again.');
        state.rows = rows; state.loaded = true; readAt = started;
        state.message = controller.state.uncertain
          ? explicit ? 'A previous speaker command is uncertain. Refresh playback before changing routines.'
            : 'An earlier command is uncertain. Use Refresh routines to inspect stored changes, or Refresh playback for a speaker command.' : '';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    async function beginCreate(action) {
      if (!editable() || !['apply_scene', 'dj_start'].includes(action)) return false;
      retire(); state.draft = null; state.options = null; state.result = null;
      const v = version, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading available routine targets…'; publish();
      try {
        const options = await S.choices(name => route(name, { signal: abort.signal }), action);
        if (!live() || v !== version || !controller.canManage() || now() - readAt >= MAX_AGE
            || now() - started >= MAX_AGE) return false;
        state.options = options; state.draft = Object.freeze(S.draftOf(action)); choicesAt = started;
        state.message = action === 'apply_scene' && !options.scenes.length
          ? 'Save a scene first, then refresh routines.' : action === 'dj_start' && !options.rooms.length
            ? 'No listening groups were reported. Refresh the house before scheduling the DJ.' : '';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function edit(field, value) {
      if (!editable() || !state.draft || !['cadence', 'time', 'days', 'delay', 'at', 'zone', 'scene', 'mood'].includes(field)
          || (field === 'days' ? !Array.isArray(value) || !value.every(day => S.DAYS.includes(day))
            : typeof value !== 'string' || value.length > 512)) return false;
      ++version; state.pending = null; state.result = null;
      state.draft = Object.freeze({ ...state.draft, [field]: Array.isArray(value) ? Object.freeze([...value]) : value });
      state.message = ''; publish(); return true;
    }
    function editDirection(field, value) {
      if (!editable() || state.draft?.action !== 'dj_start') return false;
      try {
        const direction = D.editDraft(state.draft.direction, field, value);
        ++version; state.pending = null; state.result = null;
        state.draft = Object.freeze({ ...state.draft, direction }); state.message = ''; publish(); return true;
      } catch (error) { state.message = C.problem(error); publish(); return false; }
    }
    function reviewCreate() {
      if (!editable() || !state.draft || !state.options || now() - choicesAt >= MAX_AGE) return false;
      try {
        const planned = S.creation(state.draft, state.options); S.noDuplicate(state.rows, planned);
        state.pending = Object.freeze({ ...planned, version }); state.message = ''; state.result = null; publish(); return true;
      } catch (error) { state.pending = null; state.message = C.problem(error); publish(); return false; }
    }
    function review(action, id) {
      if (!editable() || !['pause', 'resume', 'remove'].includes(action)) return false;
      const row = state.rows.find(item => item.id === id);
      if (!row || (action === 'pause' && !row.enabled) || (action === 'resume' && row.enabled)) return false;
      retire(); state.draft = null; state.options = null; state.result = null;
      state.pending = Object.freeze({ action, row, version }); state.message = ''; publish(); return true;
    }
    function cancel() {
      if (!live() || state.writing) return false;
      retire(); state.draft = null; state.options = null; state.message = ''; publish(); return true;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version
      && (state.pending.action !== 'create' || now() - choicesAt < MAX_AGE));
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      reading = abort; state.pending = null; state.writing = true; state.result = null;
      state.message = 'Checking the stored routine and its targets…'; publish();
      let result = null, ok = false;
      try {
        ok = await controller.mutate(async ({ check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || now() - readAt >= MAX_AGE
                || (reviewed.action === 'create' && now() - choicesAt >= MAX_AGE)) {
              fail('This routine review is no longer current. Refresh and review it again.');
            }
          };
          valid();
          const rows = S.schedulesOf(await call('/schedules', undefined, { signal: abort.signal })); valid();
          if (reviewed.action === 'create') {
            S.noDuplicate(rows, reviewed);
            const options = await S.choices(name => route(name, { signal: abort.signal }), reviewed.body.action); valid();
            const destination = reviewed.body.action === 'apply_scene'
              ? options.scenes.find(scene => scene.name === reviewed.body.scene)
              : options.rooms.find(room => room.target === reviewed.body.zone);
            const mood = reviewed.mood && options.moods.find(item => item.name === reviewed.mood.name);
            if (!destination || destination.signature !== reviewed.destination.signature
                || (reviewed.mood && mood?.signature !== reviewed.mood.signature)) {
              fail('The saved scene, group or DJ mood changed. Refresh and review it again.');
            }
            valid(); result = S.addedOf(await send('/schedules', reviewed.body), rows, reviewed);
          } else {
            const row = rows.find(item => item.id === reviewed.row.id);
            if (!row || S.definition(row) !== S.definition(reviewed.row) || row.enabled !== reviewed.row.enabled) {
              fail('The routine changed or was removed. Refresh and review it again.');
            }
            valid(); result = S.changedOf(await send(`/schedules/${reviewed.action}`, { id: row.id }), row, reviewed.action);
          }
          return { done: done(reviewed.action, result), changed: true };
        }, { stored: 'schedules' });
        if (live() && v === version) {
          if (ok) {
            state.rows = reviewed.action === 'create' ? [...state.rows, result]
              : reviewed.action === 'remove' ? state.rows.filter(row => row.id !== result.id)
                : state.rows.map(row => row.id === result.id ? result : row);
            state.result = Object.freeze({ action: reviewed.action, row: result });
          }
          state.message = (controller.state.message || 'The dashboard is busy. No routine was changed.')
            + (ok ? ' Refresh routines before another change.' : '');
        }
        return ok;
      } finally {
        if (reading === abort) reading = null;
        state.writing = false; state.loaded = false; state.draft = null; state.options = null; publish();
      }
    }
    return { state, canRead, editable, canApply, read, beginCreate, edit, editDirection, reviewCreate, review, cancel, apply, observe,
      open() { if (disposed || !active || state.writing) return false; state.open = true; publish(); return true; },
      close() { retire(true); state.open = false; publish(); },
      suspend() { active = false; retire(true); state.open = false; publish(); },
      resume() { if (!disposed) { active = true; publish(); } },
      dispose() { active = false; disposed = true; retire(true); state.open = false; } };
  }

  function panel({ call, controller, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('routines-dialog'), body = $('routines-body');
    let rendered = '', directionOpen = false;
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
    const focusKey = 'data-routine-focus';
    const button = (label, key, action, disabled = false, cls = 'text-button') => el('button', {
      type: 'button', class: cls, text: label, [focusKey]: key, disabled, onclick: action });
    const field = (label, input) => el('label', { class: 'routine-field', for: input.id }, label, input);
    const control = model({ call, controller, now, changed: render });
    function input(draft, key, type = 'text', placeholder = '') {
      return el('input', { id: `routine-${key}`, type, value: draft[key], maxlength: 512, placeholder,
        [focusKey]: key, disabled: !control.editable(), oninput: event => control.edit(key, event.target.value) });
    }
    function select(draft, key, choices) {
      const node = el('select', { id: `routine-${key}`, [focusKey]: key, disabled: !control.editable(),
        onchange: event => control.edit(key, event.target.value) });
      for (const [value, label] of choices) node.append(el('option', { value, text: label }));
      node.value = draft[key]; return node;
    }
    function form(state) {
      const d = state.draft, options = state.options, box = el('section', { class: 'routine-form', 'aria-label': 'New routine' });
      box.append(el('h3', { text: d.action === 'apply_scene' ? 'Schedule a saved scene' : 'Schedule the DJ' }));
      if (d.action === 'apply_scene') {
        box.append(field('Saved scene', select(d, 'scene', [['', 'Choose a saved scene'], ...options.scenes.map(scene => [scene.name, scene.name])])),
          el('p', { class: 'small-note', text: 'Each run uses the scene saved under this name at that time. Replacing the scene changes future runs; deleting it prevents a successful recall.' }));
      } else {
        box.append(field('Listening group', select(d, 'zone', [['', 'Choose a group'], ...options.rooms.map(room => [room.target, room.label])])),
          field('DJ mood', select(d, 'mood', [['', 'Automatic program'], ...options.moods.map(mood => [mood.name, mood.name])])),
          D.editor({ draft: d.direction, change: control.editDirection, disabled: !control.editable(), el,
            idPrefix: 'routine-direction', focusAttribute: focusKey, open: directionOpen, toggle: value => { directionOpen = value; } }),
          el('p', { class: 'small-note', text: 'Each run replaces temporary DJ steering with this direction. Automatic clears earlier steering. Complete works already queued remain queued.' }));
      }
      box.append(field('When', select(d, 'cadence', [['daily', 'Every day'], ['weekdays', 'Weekdays'], ['weekends', 'Weekends'],
        ['custom', 'Choose days'], ['delay', 'Once, after a delay'], ['once', 'Once, at an exact time']])));
      if (d.cadence === 'delay') box.append(field('Delay from creation', input(d, 'delay', 'text', '45m or 1h30m')));
      else if (d.cadence === 'once') box.append(field('Date, time and UTC offset', input(d, 'at', 'text', '2030-06-15T19:30:00-04:00')),
        el('p', { class: 'small-note', text: 'Include seconds and an explicit UTC offset or Z. The house validates that this instant is still in the future.' }));
      else {
        box.append(field('House time', input(d, 'time', 'time')));
        if (d.cadence === 'custom') {
          const days = el('fieldset', { class: 'routine-days' }, el('legend', { text: 'Days of the week' }));
          for (const day of S.DAYS) days.append(el('label', {}, el('input', { type: 'checkbox', checked: d.days.includes(day),
            [focusKey]: `day-${day}`, disabled: !control.editable(), onchange: event => control.edit('days',
              event.target.checked ? [...d.days, day] : d.days.filter(value => value !== day)) }), day));
          box.append(days);
        }
      }
      box.append(el('div', { class: 'routine-actions' }, button('Cancel draft', 'cancel-draft', control.cancel),
        button('Review routine', 'review-create', control.reviewCreate, !control.editable(), 'primary-button')));
      return box;
    }
    function preview(pending) {
      const box = el('section', { class: 'routine-review', tabindex: '-1', 'aria-label': 'Review routine' });
      box.append(el('h3', { text: pending.action === 'create' ? 'Review new routine' : `Review routine #${pending.row.id}` }));
      if (pending.action === 'create') {
        box.append(el('p', { text: pending.summary }), el('p', { class: 'routine-when', text: pending.body.when }),
          el('p', { class: 'small-note', text: pending.timing.kind === 'weekly'
            ? 'Recurring time uses the daemon host’s local timezone, including its daylight-saving rules.'
            : pending.timing.kind === 'delay' ? 'The delay starts when the house accepts creation. Its reply reports the exact next run.'
              : 'The explicit offset determines the instant. Its reply reports the next run in the house timezone.' }));
        if (pending.destination.details) box.append(el('details', {}, el('summary', { text: 'Scene contents reviewed now' }),
          el('pre', { text: pending.destination.details })));
        box.append(el('p', { class: 'small-note', text: 'The routine persists on the daemon and runs with its creator’s rights. Closing this page does not cancel it. No music starts during creation.' }));
      } else box.append(el('p', { text: pending.row.summary }), el('p', { text: pending.row.when }),
        el('p', { text: effect(pending.action) }));
      box.append(el('div', { class: 'routine-actions' }, button('Cancel review', 'cancel-review', control.cancel),
        button(pending.action === 'create' ? 'Create routine' : pending.action === 'pause' ? 'Pause routine'
          : pending.action === 'resume' ? 'Resume routine' : 'Remove routine', 'confirm', () => void control.apply(pending),
        !control.canApply(), 'primary-button')));
      return box;
    }
    function render(state) {
      $('open-routines').disabled = !controller.state.active || controller.state.busy;
      $('routines-read').disabled = !control.canRead();
      $('routines-create-scene').disabled = !control.editable();
      $('routines-create-dj').disabled = !control.editable();
      $('routines-message').textContent = state.message;
      const key = JSON.stringify([state.rows, state.loaded, state.loading, state.writing, state.draft,
        state.options, state.pending, state.result, control.editable(), control.canApply()]);
      if (key === rendered) return; rendered = key;
      const previous = doc.activeElement, focused = previous?.getAttribute(focusKey), caret = previous?.selectionStart;
      body.replaceChildren();
      if (state.pending) body.append(preview(state.pending));
      else if (state.draft && state.options) body.append(form(state));
      if (state.result) body.append(el('p', { class: 'routine-result', role: 'status', text: done(state.result.action, state.result.row) }));
      if (!state.loaded) body.append(el('p', { class: 'small-note', text: state.loading
        ? 'Reading the house’s stored routines…' : 'Refresh routines to review current settings before changing them.' }));
      const list = el('div', { class: 'routine-list', 'aria-label': 'Saved routines' });
      if (state.loaded && !state.rows.length) list.append(el('p', { text: 'No saved routines yet. Schedule a scene or DJ session above.' }));
      for (const row of state.rows) {
        const card = el('article', { class: 'routine-card' },
          el('div', { class: 'routine-card-heading' }, el('h3', { text: `#${row.id} · ${row.when}` }),
            el('span', { class: 'routine-status', text: row.enabled ? row.next ? 'Enabled' : 'No future run' : 'Paused' })),
          el('p', { text: row.summary }), el('p', { class: 'small-note', text: `Created by ${row.creator}` }),
          el('dl', { class: 'routine-times' }, el('dt', { text: 'Next run' }), el('dd', { text: row.next ? S.displayTime(row.next) : 'None reported by the house' }),
            el('dt', { text: 'Last fired / skipped' }), el('dd', { text: S.displayTime(row.last_fired) })),
          el('div', { class: 'routine-actions' }, button(row.enabled ? 'Pause' : 'Resume', `toggle-${row.id}`,
            () => control.review(row.enabled ? 'pause' : 'resume', row.id), !control.editable()),
          button('Remove', `remove-${row.id}`, () => control.review('remove', row.id), !control.editable())));
        list.append(card);
      }
      body.append(list);
      const next = [...body.querySelectorAll(`[${focusKey}]`)].find(node => node.getAttribute(focusKey) === focused);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (Number.isInteger(caret) && (next.type === 'text' || next.tagName === 'TEXTAREA')) next.setSelectionRange(caret, caret);
      }
    }
    function close() { control.close(); if (dialog.open) dialog.close(); $('open-routines').setAttribute('aria-expanded', 'false'); }
    $('open-routines').addEventListener('click', () => {
      if (!control.open()) return;
      dialog.showModal(); $('open-routines').setAttribute('aria-expanded', 'true'); void control.read(false);
    });
    $('close-routines').addEventListener('click', close);
    $('routines-read').addEventListener('click', () => void control.read(true));
    $('routines-create-scene').addEventListener('click', () => void control.beginCreate('apply_scene'));
    $('routines-create-dj').addEventListener('click', () => void control.beginCreate('dj_start'));
    dialog.addEventListener('cancel', event => { event.preventDefault(); close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); $('open-routines').setAttribute('aria-expanded', 'false'); });
    render(control.state);
    return { control, observe: control.observe, suspend() { control.suspend(); close(); }, resume: control.resume,
      dispose() { control.dispose(); if (dialog.open) dialog.close(); } };
  }
  return { MAX_AGE, effect, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardRoutines;
