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
    : action === 'update' ? `Saved routine #${row.id}: ${row.when}: ${row.summary}. ${row.enabled ? 'Enabled' : 'Paused'}. No music starts now.`
    : `Routine #${row.id} ${action === 'pause' ? 'paused' : action === 'resume' ? 'resumed' : 'removed'}.`;

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0, choicesAt = 0;
    const state = { open: false, rows: [], loaded: false, loading: false, writing: false,
      draft: null, options: null, editing: null, pending: null, result: null, recovery: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open && controller.state.active;
    const canRead = () => live() && !state.loading && !state.writing && !controller.state.busy;
    const editable = () => live() && state.loaded && !state.loading && !state.writing && !state.recovery
      && now() - readAt < MAX_AGE && controller.canManage();
    const route = (name, { query, ...options } = {}) => call(S.routes[name][1]
      + (query ? '?' + new URLSearchParams(query) : ''), undefined, options);
    function retire(discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null;
      if (discard) {
        state.loaded = false; state.options = null;
        if (!state.editing) state.draft = null;
        if (state.result?.action !== 'update') state.result = null;
      }
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
        if (explicit && state.recovery && !controller.state.uncertain) {
          state.result = Object.freeze({ ...state.recovery, status: 'observed', detail: S.updateObservation(rows, state.recovery.id) });
          state.recovery = null;
        }
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
    async function loadDraft(draft, row = null) {
      if (!editable()) return false;
      retire(); state.draft = row ? Object.freeze(draft) : null; state.editing = row; state.options = null;
      if (state.result?.action !== 'update') state.result = null;
      const v = version, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading available routine targets…'; publish();
      try {
        const options = await S.choices((name, args) => route(name, { ...args, signal: abort.signal }), draft.action, row);
        if (!live() || v !== version || !controller.canManage() || now() - readAt >= MAX_AGE
            || now() - started >= MAX_AGE) return false;
        state.options = options; state.draft = Object.freeze(draft); choicesAt = started;
        state.message = draft.action === 'apply_scene' && !options.scenes.length
          ? 'Save a scene first, then refresh routines.' : draft.action === 'dj_start' && !options.rooms.length
            ? 'No listening groups were reported. Refresh the house before scheduling the DJ.' : '';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function beginCreate(action = 'dj_start') {
      if (!['apply_scene', 'dj_start', 'pause', 'volume', 'announce'].includes(action)) return Promise.resolve(false);
      return loadDraft(S.draftOf(action));
    }
    function beginEdit(id) {
      if (!editable()) return Promise.resolve(false);
      const row = state.rows.find(item => item.id === id);
      try { return loadDraft(S.draftFromSchedule(row), row); }
      catch (error) { state.message = C.problem(error); publish(); return Promise.resolve(false); }
    }
    function changeAction(action) {
      if (!state.draft) return Promise.resolve(false);
      try { return loadDraft(S.actionDraft(state.draft, action), state.editing); }
      catch (error) { state.message = C.problem(error); publish(); return Promise.resolve(false); }
    }
    function reloadDraft() { return state.draft ? loadDraft(state.draft, state.editing) : Promise.resolve(false); }
    function edit(field, value) {
      if (!editable() || !state.draft || !Object.hasOwn(state.draft, field)
          || ['action', 'direction', 'original_direction', 'direction_edited', 'direction_duration_edited'].includes(field)
          || (field === 'days' ? !Array.isArray(value) || !value.every(day => S.DAYS.includes(day))
            : field === 'announcement_rooms' ? !Array.isArray(value) || !value.every(target => typeof target === 'string' && [...target].length <= 128)
              : field === 'announcement_all' ? typeof value !== 'boolean'
                : typeof value !== 'string' || value.length > (field === 'announcement_text' ? 2000 : field === 'announcement_voice' ? 4096 : 512))) return false;
      ++version; state.pending = null;
      state.draft = Object.freeze({ ...state.draft, [field]: Array.isArray(value) ? Object.freeze([...value]) : value });
      state.message = ''; publish(); return true;
    }
    function editDirection(field, value) {
      if (!editable() || state.draft?.action !== 'dj_start') return false;
      try {
        const direction = D.editDraft(state.draft.direction, field, value);
        ++version; state.pending = null;
        state.draft = Object.freeze({ ...state.draft, direction, direction_edited: true,
          direction_duration_edited: state.draft.direction_duration_edited || field === 'minutes' });
        state.message = ''; publish(); return true;
      } catch (error) { state.message = C.problem(error); publish(); return false; }
    }
    function reviewCreate() {
      if (!editable() || !state.draft || !state.options || now() - choicesAt >= MAX_AGE) return false;
      try {
        const planned = state.editing ? S.updatePlan(state.editing, state.draft, state.options, state.rows) : S.creation(state.draft, state.options);
        if (!state.editing) S.noDuplicate(state.rows, planned);
        if (planned.body.action === 'dj_start') {
          const v = ++version, abort = new AbortController();
          reading = abort; state.pending = null; state.loading = true;
          state.message = 'Reading every group in the scheduled DJ target…'; publish();
          return S.reviewTargets((name, args) => route(name, { ...args, signal: abort.signal }), planned).then(reviewed => {
            if (!live() || v !== version || !controller.canManage() || now() - readAt >= MAX_AGE
                || now() - choicesAt >= MAX_AGE) return false;
            state.pending = Object.freeze({ ...reviewed, version }); state.message = ''; return true;
          }).catch(error => { if (live() && v === version) state.message = C.problem(error); return false; })
            .finally(() => { if (reading === abort) { reading = null; state.loading = false; publish(); } });
        }
        state.pending = Object.freeze({ ...planned, version }); state.message = ''; publish(); return true;
      } catch (error) { state.pending = null; state.message = C.problem(error); publish(); return false; }
    }
    function review(action, id) {
      if (!editable() || !['pause', 'resume', 'remove'].includes(action)) return false;
      const row = state.rows.find(item => item.id === id);
      if (!row || (action === 'pause' && !row.enabled) || (action === 'resume' && row.enabled)) return false;
      retire(); state.draft = null; state.editing = null; state.options = null;
      if (state.result?.action !== 'update') state.result = null;
      state.pending = Object.freeze({ action, row, version }); state.message = ''; publish(); return true;
    }
    function dismissCreate() {
      if (!live() || state.writing) return false;
      retire(); state.draft = null; state.editing = null; state.options = null; state.message = ''; publish(); return true;
    }
    function cancel() {
      if (!live() || state.writing) return false;
      if (!state.editing) return dismissCreate();
      retire(); state.message = ''; publish(); return true;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version
      && (!['create', 'update'].includes(state.pending.action) || now() - choicesAt < MAX_AGE));
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController(), updating = reviewed.action === 'update';
      reading = abort; state.pending = null; state.writing = true;
      if (state.result?.action !== 'update') state.result = null;
      state.message = 'Checking the stored routine and its targets…'; publish();
      let result = null, ok = false, sent = false;
      try {
        ok = await controller.mutate(async ({ check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || now() - readAt >= MAX_AGE
                || (['create', 'update'].includes(reviewed.action) && now() - choicesAt >= MAX_AGE)) {
              fail('This routine review is no longer current. Refresh and review it again.');
            }
          };
          valid();
          const rows = S.schedulesOf(await call('/schedules', undefined, { signal: abort.signal })); valid();
          if (reviewed.action === 'create' || updating) {
            if (updating) S.editUnchanged(rows.find(row => row.id === reviewed.row.id), reviewed.row);
            S.noDuplicate(rows.filter(row => !updating || row.id !== reviewed.row.id), reviewed);
            const options = await S.choices((name, args) => route(name, { ...args, signal: abort.signal }), reviewed.body.action,
              updating ? reviewed.row : null, reviewed.body.action === 'dj_start' ? reviewed.body.zone : null); valid();
            S.targetsUnchanged(reviewed, options, 'The saved scene, group or DJ mood changed. Refresh and review it again.');
            valid(); sent = true;
            result = updating ? S.updatedOf(await send('/schedules/update', reviewed.request), reviewed)
              : S.addedOf(await send('/schedules', reviewed.body), rows, reviewed);
          } else {
            const row = rows.find(item => item.id === reviewed.row.id);
            if (!row || S.definition(row) !== S.definition(reviewed.row) || row.enabled !== reviewed.row.enabled) {
              fail('The routine changed or was removed. Refresh and review it again.');
            }
            valid(); sent = true; result = S.changedOf(await send(`/schedules/${reviewed.action}`, { id: row.id }), row, reviewed.action);
          }
          return { done: done(reviewed.action, result), changed: true };
        }, { stored: 'schedules' });
        if (updating) {
          state.result = Object.freeze({ action: 'update', status: ok ? 'accepted' : sent ? 'uncertain' : 'not_sent',
            id: reviewed.row.id, row: ok ? result : reviewed.row,
            detail: ok ? done('update', result) : (controller.state.message || 'No routine edit was sent.')
              + ' Your draft is retained; refresh routines before a new review.' });
          if (sent && !ok) state.recovery = state.result;
          if (ok) { state.editing = null; state.draft = null; }
        }
        if (live() && v === version) {
          if (ok) {
            state.rows = reviewed.action === 'create' ? [...state.rows, result]
              : reviewed.action === 'remove' ? state.rows.filter(row => row.id !== result.id)
                : state.rows.map(row => row.id === result.id ? result : row);
            if (!updating) state.result = Object.freeze({ action: reviewed.action, row: result });
          }
          state.message = (controller.state.message || 'The dashboard is busy. No routine was changed.')
            + (ok ? ' Refresh routines before another change.' : '');
        }
        return ok;
      } finally {
        if (reading === abort) reading = null;
        state.writing = false; state.loaded = false; state.options = null;
        if (!state.editing) state.draft = null;
        publish();
      }
    }
    return { state, canRead, editable, canApply, read, beginCreate, beginEdit, changeAction, reloadDraft, edit, editDirection,
      reviewCreate, review, cancel, dismissCreate, apply, observe,
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
    const control = model({ call, controller, now, changed: render });
    function form(state) {
      return S.routineForm({ state, control, el, button, idPrefix: 'routine', focusAttribute: focusKey,
        className: 'routine-form', disabled: !control.editable(), directionOpen,
        toggleDirection: value => { directionOpen = value; } });
    }
    function preview(pending) {
      const box = el('section', { class: 'routine-review', tabindex: '-1', 'aria-label': 'Review routine' });
      box.append(el('h3', { text: pending.action === 'create' ? 'Review new routine' : `Review routine #${pending.row.id}` }));
      if (pending.action === 'create' || pending.action === 'update') {
        if (pending.action === 'update') box.append(el('p', { text: `Previously: ${pending.row.when}: ${pending.row.summary}` }));
        box.append(el('p', { text: pending.summary }), el('p', { class: 'routine-when', text: pending.body.when }),
          el('p', { class: 'small-note', text: pending.timing.kind === 'weekly'
            ? 'Recurring time uses the daemon host’s local timezone, including its daylight-saving rules.'
            : pending.timing.kind === 'delay' ? 'The delay starts when the house accepts this routine. Its reply reports the exact next run.'
              : 'The explicit offset determines the instant. Its reply reports the next run in the house timezone.' }));
        if (pending.destination.details) box.append(el('details', {}, el('summary', { text: 'Scene contents reviewed now' }),
          el('pre', { text: pending.destination.details })));
        const targets = S.routineTargets(pending, el); if (targets) box.append(targets);
        box.append(el('p', { class: 'small-note', text: pending.action === 'update' ? S.updateEffect(pending.row) : 'The routine persists on the daemon and runs with its creator’s rights. Closing this page does not cancel it. No music starts during creation.' }));
      } else box.append(el('p', { text: pending.row.summary }), el('p', { text: pending.row.when }),
        el('p', { text: effect(pending.action) }));
      box.append(el('div', { class: 'routine-actions' }, button('Cancel review', 'cancel-review', control.cancel),
        button(pending.action === 'create' ? 'Create routine' : pending.action === 'update' ? 'Save routine changes' : pending.action === 'pause' ? 'Pause routine'
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
        state.options, state.editing, state.pending, state.result, state.recovery, control.editable(), control.canApply()]);
      if (key === rendered) return; rendered = key;
      const previous = doc.activeElement, focused = previous?.getAttribute(focusKey), caret = previous?.selectionStart;
      body.replaceChildren();
      if (state.pending) body.append(preview(state.pending));
      else if (state.draft) body.append(form(state));
      if (state.result) body.append(el('p', { class: 'routine-result', role: 'status', text: state.result.detail || done(state.result.action, state.result.row) }));
      if (!state.loaded) body.append(el('p', { class: 'small-note', text: state.loading
        ? 'Reading the house’s stored routines…' : 'Refresh routines to review current settings before changing them.' }));
      const list = el('div', { class: 'routine-list', 'aria-label': 'Saved routines' });
      if (state.loaded && !state.rows.length) list.append(el('p', { text: 'No saved routines yet. Create a routine above.' }));
      if (state.loaded && state.rows.some(row => !row.revision)) list.append(el('p', { class: 'small-note', text: 'Editing needs a refreshed list from an updated daemon that provides routine revisions.' }));
      for (const row of state.rows) {
        const card = el('article', { class: 'routine-card' },
          el('div', { class: 'routine-card-heading' }, el('h3', { text: `#${row.id} · ${row.when}` }),
            el('span', { class: 'routine-status', text: row.enabled ? row.next ? 'Enabled' : 'No future run' : 'Paused' })),
          el('p', { text: row.summary }), el('p', { class: 'small-note', text: `Created by ${row.creator}` }),
          el('dl', { class: 'routine-times' }, el('dt', { text: 'Next run' }), el('dd', { text: row.next ? S.displayTime(row.next) : 'None reported by the house' }),
            el('dt', { text: 'Last occurrence / future-run cutoff' }), el('dd', { text: S.displayTime(row.last_fired) })),
          el('div', { class: 'routine-actions' },
            button('Edit', `edit-${row.id}`, () => void control.beginEdit(row.id), !control.editable() || !row.revision),
            button(row.enabled ? 'Pause' : 'Resume', `toggle-${row.id}`,
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
