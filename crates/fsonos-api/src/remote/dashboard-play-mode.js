// Playback order belongs to the native queue, with the dashboard's shared command lane.
'use strict';
const DashboardPlayMode = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const MODES = Object.freeze({
    NORMAL: 'In order · no repeat',
    REPEAT_ALL: 'In order · repeat queue',
    REPEAT_ONE: 'Repeat current track',
    SHUFFLE_NOREPEAT: 'Shuffle · no repeat',
    SHUFFLE: 'Shuffle · repeat queue',
    SHUFFLE_REPEAT_ONE: 'Shuffle · repeat current track',
  });
  const MAX_AGE = 45000;
  const known = value => typeof value === 'string' && Object.hasOwn(MODES, value);
  const fail = message => { throw new Error(message); };
  function observed(value, zone) {
    if (!value || value.zone !== zone.coordinator_room || !known(value.mode)
        || typeof value.queue_active !== 'boolean' || typeof value.dj_active !== 'boolean') {
      fail('The playback-order response is incomplete or unsupported. Read it again.');
    }
    return Object.freeze({ zone: value.zone, mode: value.mode,
      queue_active: value.queue_active, dj_active: value.dj_active });
  }
  const same = (a, b) => a.mode === b.mode && a.queue_active === b.queue_active && a.dj_active === b.dj_active;
  function confirmed(value, zone, mode) {
    const state = observed(value?.state, zone);
    if (typeof value.done !== 'string' || !value.done.trim() || typeof value.changed !== 'boolean'
        || typeof value.recovered !== 'boolean' || state.mode !== mode || !state.queue_active
        || (state.dj_active && mode !== 'NORMAL')) fail('The requested playback order was not confirmed.');
    return value;
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', label: '', current: null, desired: '',
      pending: null, loading: false, writing: false, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const item = () => controller.selected();
    const live = () => !disposed && active && state.open;
    const matching = () => item() && state.target === C.key(item().zone) && state.scope === C.shape(item().zone);
    const canRead = () => !!(live() && item()?.fresh && controller.state.active
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.current && !state.loading && !state.writing
      && now() - readAt < MAX_AGE && controller.canControl());
    function retire(message) {
      ++version;
      reading?.abort(); reading = null; state.loading = false;
      state.current = null; state.desired = ''; state.pending = null; readAt = 0;
      if (message) state.message = message;
    }
    function scope() {
      const selected = item();
      state.target = selected ? C.key(selected.zone) : '';
      state.scope = selected ? C.shape(selected.zone) : '';
      state.label = selected ? selected.zone.members.join(' + ') + ' · ' + selected.zone.household : 'Choose a listening group';
    }
    function observe() {
      if (disposed) return;
      if (!matching()) { retire('The selected group changed. Read its playback order again.'); scope(); }
      else if ((state.current || state.loading || state.pending)
          && (!item()?.fresh || !controller.state.active || (state.current && now() - readAt >= MAX_AGE))) {
        retire('Playback order needs a fresh observation. Refresh playback if it is stale, then read the setting.');
      }
      publish();
    }
    async function read() {
      if (!canRead()) return false;
      retire(); scope();
      const v = version, zone = item().zone, abort = new AbortController();
      reading = abort; state.loading = true; state.message = 'Reading playback order…'; state.result = null; publish();
      try {
        const value = observed(await call('/zones/' + encodeURIComponent(state.target) + '/play-mode',
          undefined, { signal: abort.signal }), zone);
        if (!live() || v !== version || !matching()) return false;
        state.current = value; state.desired = value.mode; readAt = now(); state.message = '';
        return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function choose(mode) {
      if (!editable() || !known(mode) || !state.current.queue_active
          || (state.current.dj_active && mode !== 'NORMAL')) return false;
      state.desired = mode; state.result = null; state.message = '';
      state.pending = mode === state.current.mode ? null : Object.freeze({
        version, target: state.target, scope: state.scope, label: state.label,
        current: state.current, mode,
      });
      publish(); return true;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version
      && state.pending.current === state.current && state.current.queue_active
      && (!state.current.dj_active || state.pending.mode === 'NORMAL'));
    async function apply(review = state.pending) {
      if (!canApply() || review !== state.pending) return false;
      const v = version;
      let outcome = null;
      state.pending = null; state.writing = true; state.message = 'Checking the group and current playback order…';
      state.result = null; publish();
      try {
        const ok = await controller.mutate(async ({ item: selected, target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || !matching() || target !== review.target
                || C.shape(selected.zone) !== review.scope) fail('The playback-order review is no longer current.');
          };
          valid();
          C.playback(await call('/zones/' + encodeURIComponent(target) + '/state'), selected.zone);
          valid();
          const fresh = observed(await call('/zones/' + encodeURIComponent(target) + '/play-mode'), selected.zone);
          valid();
          if (!same(fresh, review.current)) fail('The source, DJ or playback order changed. Read the setting and choose again.');
          const result = confirmed(await send('/play-mode', {
            zone: target, mode: review.mode, expected_mode: review.current.mode,
          }), selected.zone, review.mode);
          outcome = Object.freeze({ target, label: review.label, mode: review.mode,
            done: result.done, recovered: result.recovered });
          return result;
        }, { wholeHouse: false });
        if (ok) {
          state.result = outcome;
          state.message = outcome.done + ' Read the current setting before making another change.';
        } else state.message = controller.state.message || 'No change was sent. Refresh playback and read the setting again.';
        return ok;
      } finally {
        state.writing = false;
        retire();
        publish();
      }
    }
    return { state, read, choose, apply, canRead, canApply, editable, observe,
      open() {
        if (disposed || !active) return Promise.resolve(false);
        state.open = true; scope(); publish(); return read();
      },
      close() { state.open = false; retire(); publish(); },
      suspend() { active = false; state.open = false; retire(); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(); },
    };
  }

  function panel({ call, controller, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('play-mode-dialog'), trigger = $('open-play-mode');
    let opener = null;
    const control = model({ call, controller, now, changed: render });
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('play-mode-target').textContent = s.label;
      $('play-mode-read').disabled = !control.canRead();
      const value = s.current;
      $('play-mode-current').textContent = value ? 'Current: ' + MODES[value.mode] : 'Current setting not yet read.';
      $('play-mode-description').textContent = !value ? 'Read the selected group’s queue settings before choosing.'
        : !value.queue_active ? 'Another source is selected. Play an existing queue entry first; this control will not switch sources.'
          : value.dj_active ? 'The DJ keeps whole works in sequence. Stop the DJ before selecting shuffle or repeat.'
            : 'Choose how the speaker plays its current queue. This does not restart the music.';
      const choice = $('play-mode-choice');
      choice.disabled = !control.editable() || !value?.queue_active;
      for (const option of choice.options) option.disabled = !!(value?.dj_active && option.value !== 'NORMAL');
      choice.value = s.desired || 'NORMAL';
      $('play-mode-apply').disabled = !control.canApply();
      $('play-mode-apply').textContent = s.writing ? 'Applying…' : 'Apply playback order';
      $('play-mode-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
    }
    trigger.addEventListener('click', () => {
      opener = doc.activeElement; void control.open();
    });
    $('play-mode-read').addEventListener('click', () => { void control.read(); });
    $('play-mode-choice').addEventListener('change', event => control.choose(event.target.value));
    $('play-mode-apply').addEventListener('click', () => { void control.apply(); });
    $('close-play-mode').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return control;
  }
  return { MODES, MAX_AGE, observed, confirmed, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardPlayMode;

