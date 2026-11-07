// The speaker/daemon owns the deadline; the dashboard only reviews native timer changes.
'use strict';
const DashboardSleep = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const T = typeof TimersRemote !== 'undefined' ? TimersRemote : require('./timers.js');
  const MAX_AGE = 45000, MAX_MINUTES = 23 * 60, PRESETS = Object.freeze([15, 30, 45, 60, 90, 120]);
  const fail = message => { throw new Error(message); };
  function minutesOf(value) {
    if (typeof value !== 'string' || !/^\d{1,4}$/.test(value.trim())) return null;
    const minutes = Number(value.trim());
    return minutes >= 1 && minutes <= MAX_MINUTES ? minutes : null;
  }
  function timerOf(value, zone) {
    if (!Array.isArray(value) || value.length > 1) fail('The timer list is unreadable. Read it again.');
    return value.length ? T.timerOf(value[0], { coordinator: zone.coordinator_room }) : null;
  }
  function groupOf(value, target, scope) {
    const zone = C.zones(value).find(zone => C.key(zone) === target);
    if (!zone || C.shape(zone) !== scope) fail('The group or its coordinator changed. Refresh playback and read the timer again.');
    return zone;
  }
  function confirmed(value, zone, review) {
    const timer = T.timerOf(value, { coordinator: zone.coordinator_room });
    if ((review.action === 'cancel') !== (timer === null)
        || (timer && (timer.fading || !value.changed))
        || (review.action === 'set' && timer?.remaining !== review.minutes * 60)) {
      fail('The reply did not confirm the requested timer change.');
    }
    return value;
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const state = { open: false, target: '', scope: '', label: '', timer: null, loaded: false, stale: true,
      loading: false, writing: false, minutes: '45', pending: null, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => controller.selected();
    const live = () => !disposed && active && state.open;
    const matching = () => selected() && state.target === C.key(selected().zone) && state.scope === C.shape(selected().zone);
    const canRead = () => !!(live() && selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.loaded && !state.stale && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    function scope() {
      const item = selected();
      state.target = item ? C.key(item.zone) : '';
      state.scope = item ? C.shape(item.zone) : '';
      state.label = item ? item.zone.members.join(' + ') + ' · ' + item.zone.household : 'Choose a listening group';
    }
    function retire(message, discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false;
      state.pending = null; state.stale = true;
      if (discard) { state.timer = null; state.loaded = false; readAt = 0; }
      if (message) state.message = message;
    }
    function observe() {
      if (disposed) return;
      if (!matching()) { retire('The selected group changed. Read its timer again.', true); scope(); }
      else if ((state.loading || state.pending || (state.loaded && !state.stale))
          && (!selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (state.loaded && !state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read the current timer before making another change. Refresh playback first if the group is stale.');
      }
      publish();
    }
    async function read(keepResult = false) {
      if (!canRead()) return false;
      retire(); scope();
      const v = version, target = state.target, shape = state.scope, abort = new AbortController();
      reading = abort; state.loading = true;
      if (!keepResult) state.result = null;
      state.message = state.result ? state.result.done + ' Reading the current timer…' : 'Reading the current timer…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh) fail('The timer observation is no longer current.');
      };
      try {
        const options = { signal: abort.signal };
        const zone = groupOf(await call('/zones', undefined, options), target, shape); valid();
        const timer = timerOf(await call('/sleep?zone=' + encodeURIComponent(target), undefined, options), zone); valid();
        groupOf(await call('/zones', undefined, options), target, shape); valid();
        state.timer = timer; state.loaded = true; state.stale = false; readAt = now();
        state.message = state.result?.done || ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = [state.result?.done, C.problem(error)].filter(Boolean).join(' ');
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    const remaining = () => state.loaded && state.timer
      ? Math.max(0, state.timer.remaining - Math.floor(Math.max(0, now() - readAt) / 1000)) : null;
    function minutes(value) {
      if (!editable() || typeof value !== 'string') return false;
      ++version; state.minutes = value; state.pending = null; state.result = null; state.message = ''; publish(); return true;
    }
    function canReview(action) {
      if (!editable() || !['set', 'extend', 'cancel'].includes(action)) return false;
      if (action === 'cancel') return !!state.timer;
      const duration = minutesOf(state.minutes);
      return duration !== null && (action !== 'extend' || (!!state.timer && remaining() + duration * 60 <= MAX_MINUTES * 60));
    }
    function review(action) {
      if (!canReview(action)) return null;
      const duration = minutesOf(state.minutes);
      const title = action === 'cancel' ? 'Cancel the sleep timer' : action === 'extend'
        ? `Extend the timer by ${duration} minutes` : `Set a timer for ${duration} minutes`;
      const detail = action === 'cancel'
        ? 'The timer will no longer pause this group. Cancelling during a fade ends that fade; it does not restart music.'
        : action === 'extend' ? 'Add this time to the existing deadline for the whole group. If it is fading, the fade ends and is rescheduled.'
          : `${state.timer ? 'Replace the existing timer. ' : ''}Pause the whole group after this duration. The daemon fades supported timers before pausing; a speaker-only timer pauses without a fade.`;
      const body = action === 'cancel' ? { zone: state.target, cancel: true }
        : { zone: state.target, duration: duration + 'm', ...(action === 'extend' ? { extend: true } : {}) };
      state.pending = Object.freeze({ version, target: state.target, scope: state.scope, label: state.label,
        action, minutes: duration, timer: state.timer, title, detail, body: Object.freeze(body) });
      state.result = null; state.message = ''; publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version && state.pending.timer === state.timer);
    async function apply(pending = state.pending) {
      if (!canApply() || pending !== state.pending) return false;
      const v = version;
      let outcome = null, ok = false;
      state.pending = null; state.writing = true; state.result = null; state.message = 'Checking the group and its current timer…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || !matching() || target !== pending.target || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('The timer review is no longer current. Read the timer and review again.');
          };
          valid();
          const zone = groupOf(await call('/zones'), target, pending.scope); valid();
          const timer = timerOf(await call('/sleep?zone=' + encodeURIComponent(target)), zone); valid();
          groupOf(await call('/zones'), target, pending.scope); valid();
          if (!T.sameTimer(timer, pending.timer)) fail('The timer or its fade state changed. Read the timer and review again.');
          if (pending.action === 'extend' && timer.remaining + pending.minutes * 60 > MAX_MINUTES * 60) {
            fail('The extended timer would exceed 23 hours. Choose a shorter extension.');
          }
          const result = confirmed(await send('/sleep', pending.body), zone, pending);
          outcome = Object.freeze({ target, label: pending.label, action: pending.action, done: result.done });
          return result;
        }, { wholeHouse: false });
        if (ok) { state.result = outcome; state.message = outcome.done; }
        else state.message = controller.state.message || 'No timer change was sent. Refresh playback and read the timer again.';
      } finally {
        state.writing = false; retire(); publish();
      }
      // One observation after an accepted command, never a timer poll or a write retry.
      if (ok && state.target === pending.target && state.scope === pending.scope && canRead()) await read(true);
      return ok;
    }
    return { state, canRead, editable, read, remaining, minutes, canReview, review, canApply, apply, observe,
      cancelReview() { if (state.writing) return false; ++version; state.pending = null; publish(); return true; },
      open() {
        if (disposed || !active) return Promise.resolve(false);
        retire(undefined, true); state.open = true; scope(); publish(); return read();
      },
      close() { state.open = false; retire(undefined, true); publish(); },
      suspend() { active = false; state.open = false; retire(undefined, true); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(undefined, true); },
    };
  }

  function panel({ call, controller, document: doc = document, now }) {
    const $ = id => doc.getElementById(id), dialog = $('sleep-dialog'), trigger = $('open-sleep');
    let opener = null, shown = null, reviewFocus = null;
    const control = model({ call, controller, now, changed: render });
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('sleep-target').textContent = s.label;
      $('sleep-read').disabled = !control.canRead();
      $('sleep-current').textContent = !s.loaded ? 'Current timer not yet read.' : !s.timer ? 'No active sleep timer.'
        : s.timer.fading ? 'Fading now' : 'Sleep timer is set';
      $('sleep-deadline').textContent = s.timer ? 'Pauses at ' + T.displayTime(s.timer.ends) + ' · your time' : '';
      const left = control.remaining();
      $('sleep-countdown').textContent = left === null ? '' : left === 0 ? 'Due now — read the current timer' : 'About ' + C.clock(left) + ' remaining';
      $('sleep-fade').textContent = !s.timer ? '' : s.timer.fades
        ? 'The daemon fades the group before pausing.' : 'Speaker timer · pauses without a fade.';
      $('sleep-freshness').textContent = s.loaded && s.stale ? 'Last observation. Read the current timer before changing it.' : '';
      $('sleep-minutes').disabled = !control.editable(); $('sleep-minutes').value = s.minutes;
      for (const value of PRESETS) {
        const button = $('sleep-preset-' + value); button.disabled = !control.editable();
        button.setAttribute('aria-pressed', String(minutesOf(s.minutes) === value));
      }
      for (const action of ['set', 'extend', 'cancel']) $('sleep-' + action).disabled = !control.canReview(action);
      $('sleep-duration-help').textContent = minutesOf(s.minutes) === null ? 'Enter whole minutes from 1 to 1,380 (23 hours).'
        : s.timer && !control.canReview('extend') && control.editable() ? 'An extension must keep the total remaining time within 23 hours.'
          : 'Use this duration to set a timer or add time to the current one.';
      const preview = $('sleep-preview'); preview.hidden = !s.pending;
      $('sleep-preview-title').textContent = s.pending?.title || '';
      $('sleep-preview-detail').textContent = s.pending ? s.pending.detail + ' Group: ' + s.pending.label + '.' : '';
      $('sleep-confirm').disabled = !control.canApply();
      $('sleep-confirm').textContent = s.writing ? 'Applying…' : 'Confirm timer change';
      $('sleep-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('sleep-read').addEventListener('click', () => { void control.read(); });
    $('sleep-minutes').addEventListener('input', event => control.minutes(event.target.value));
    for (const value of PRESETS) $('sleep-preset-' + value).addEventListener('click', () => control.minutes(String(value)));
    for (const action of ['set', 'extend', 'cancel']) $('sleep-' + action).addEventListener('click', () => control.review(action));
    $('sleep-back').addEventListener('click', () => control.cancelReview());
    $('sleep-confirm').addEventListener('click', () => { void control.apply(); });
    $('close-sleep').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return control;
  }
  return { MAX_AGE, MAX_MINUTES, PRESETS, minutesOf, timerOf, confirmed, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardSleep;
