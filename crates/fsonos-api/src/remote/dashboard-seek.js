// Reviewed track-position changes share the dashboard's native mutation lane.
'use strict';
const DashboardSeek = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const seconds = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;

  function observation(item) {
    if (!item?.state) return null;
    try {
      const state = C.playback(item.state, item.zone), track = state.track;
      if (!track || track.uri === 'NOT_IMPLEMENTED' || !seconds(track.position_secs)
          || !seconds(track.duration_secs) || track.duration_secs === 0
          || !['playing', 'paused'].includes(state.transport_state)) return null;
      const target = C.key(item.zone), group = C.shape(item.zone);
      return Object.freeze({ target, group, duration: track.duration_secs,
        identity: JSON.stringify([group, track.uri, track.queue_position ?? null,
          track.duration_secs, state.transport_state]),
        title: typeof track.title === 'string' && track.title.trim() ? track.title : 'Current track',
        rooms: item.zone.members.join(' + ') + ' · ' + item.zone.household,
        transport: state.transport_state });
    } catch (_) { return null; }
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let active = true, disposed = false, version = 0;
    const state = { open: false, pending: null, choice: null, writing: false, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const available = () => !disposed && active && !state.writing
      && controller.canControl() && observation(controller.selected()) !== null;
    const matches = choice => !!choice && observation(controller.selected())?.identity === choice.identity
      && controller.state.selected === choice.target;
    const reviewable = () => available() && state.open && matches(state.pending);
    function reject() {
      ++version; state.pending = null;
      state.message = 'Playback changed. Close and choose a position from the fresh track.';
    }
    function observe() {
      if (state.pending && !state.writing && (!available() || !matches(state.pending))) reject();
      publish();
    }
    function begin(value = null) {
      if (!available()) return null;
      const item = controller.selected(), sample = observation(item);
      const to = value === null ? Math.min(sample.duration - 1, C.elapsed(item, now())) : value;
      if (!seconds(to) || to >= sample.duration) return null;
      ++version;
      const choice = Object.freeze({ ...sample, to_secs: to });
      state.pending = state.choice = choice;
      state.open = true; state.message = ''; publish();
      return choice;
    }
    function choose(value) {
      if (!reviewable() || !seconds(value) || value >= state.pending.duration) return null;
      ++version;
      state.pending = state.choice = Object.freeze({ ...state.pending, to_secs: value });
      publish(); return state.pending;
    }
    function step(delta) {
      if (![-15, 15].includes(delta) || !reviewable()) return null;
      return choose(Math.max(0, Math.min(state.pending.duration - 1, state.pending.to_secs + delta)));
    }
    function close() {
      ++version; state.open = false; state.pending = state.choice = null; state.message = ''; publish();
    }
    async function confirm(choice = state.pending) {
      if (!choice || choice !== state.pending || !reviewable()) return false;
      const current = ++version;
      state.pending = null; state.writing = true; state.message = 'Checking the current track…'; publish();
      try {
        const accepted = await controller.mutate(async ({ item, target, check, send }) => {
          const valid = () => {
            check();
            if (disposed || !active || current !== version || !state.open
                || target !== choice.target || C.shape(item.zone) !== choice.group) {
              throw new Error('The view changed. Choose a new playback position.');
            }
          };
          valid();
          const fresh = observation({ zone: item.zone,
            state: await call('/zones/' + encodeURIComponent(target) + '/state') });
          valid();
          if (!fresh || fresh.identity !== choice.identity || choice.to_secs >= fresh.duration) {
            throw new Error('The track, queue occurrence or playback state changed. Choose a new position.');
          }
          // The speaker has no atomic seek precondition. Check once immediately
          // before the native request; never retry or add an implicit resume.
          return send('/seek', { zone: target, to_secs: choice.to_secs });
        }, { wholeHouse: false });
        if (current === version && state.open) {
          if (accepted) { state.open = false; state.choice = null; state.message = ''; }
          else state.message = controller.state.message || 'The seek was not confirmed. Refresh before choosing again.';
        }
        return accepted;
      } finally { state.writing = false; publish(); }
    }
    return { state, available, reviewable, observe, begin, choose, step, close, confirm,
      suspend() { active = false; close(); },
      resume() { if (!disposed) { active = true; publish(); } },
      dispose() { active = false; close(); disposed = true; } };
  }

  function create({ call, controller, document: doc = document, now = () => performance.now() }) {
    const $ = id => doc.getElementById(id), dialog = $('seek-dialog'), progress = $('progress');
    let control, draft = null, opener = null;
    const repaint = state => {
      const item = controller.selected(), sample = observation(item);
      const available = control.available(), at = C.elapsed(item, now());
      if (!available || sample?.identity !== draft?.identity) draft = null;
      const position = draft ? draft.value : at;
      progress.max = sample ? sample.duration - 1 : 1;
      progress.value = position === null || !sample ? 0 : Math.min(position, sample.duration - 1);
      progress.disabled = !available;
      progress.setAttribute('aria-valuetext', sample
        ? C.clock(Number(progress.value)) + ' of ' + C.clock(sample.duration)
        : 'Position unavailable');
      progress.setAttribute('aria-label', available ? 'Choose playback position' : 'Playback position unavailable');
      $('open-seek').disabled = !available;
      $('open-seek').title = available ? 'Choose a position in this track'
        : 'Seeking needs fresh playing or paused audio with a known duration.';

      if (!state.open) {
        if (dialog.open) dialog.close();
        return;
      }
      const choice = state.choice, enabled = control.reviewable();
      $('seek-track').textContent = choice.title;
      $('seek-group').textContent = choice.rooms;
      $('seek-position').textContent = C.clock(choice.to_secs);
      $('seek-duration').textContent = C.clock(choice.duration);
      $('seek-range').max = choice.duration - 1;
      $('seek-range').value = choice.to_secs;
      $('seek-range').disabled = !enabled;
      $('seek-range').setAttribute('aria-valuetext', C.clock(choice.to_secs) + ' of ' + C.clock(choice.duration));
      $('seek-back').disabled = !enabled || choice.to_secs === 0;
      $('seek-forward').disabled = !enabled || choice.to_secs >= choice.duration - 1;
      $('seek-confirm').disabled = !enabled;
      $('seek-confirm').textContent = state.writing ? 'Seeking…' : 'Seek to ' + C.clock(choice.to_secs);
      $('seek-note').textContent = choice.transport === 'paused'
        ? 'This track stays paused. Only its position changes.'
        : 'Playback continues from the selected position.';
      $('seek-status').textContent = state.message;
      $('seek-status').hidden = !state.message;
      $('seek-cancel').textContent = state.writing || !state.pending ? 'Close' : 'Cancel';
      if (!dialog.open) {
        opener = doc.activeElement;
        dialog.showModal();
        $('seek-range').focus();
      }
    };
    control = model({ call, controller, now, changed: repaint });
    const begin = value => {
      if (doc.querySelector('dialog[open]')) return;
      control.begin(value);
    };
    $('open-seek').addEventListener('click', () => begin());
    progress.addEventListener('input', () => {
      const sample = observation(controller.selected());
      if (!control.available() || !sample) { draft = null; control.observe(); return; }
      draft = { identity: sample.identity, value: Number(progress.value) };
      progress.setAttribute('aria-valuetext', C.clock(draft.value) + ' of ' + C.clock(sample.duration));
    });
    progress.addEventListener('change', () => {
      const sample = observation(controller.selected());
      const value = Number(progress.value), valid = !!draft && sample?.identity === draft.identity;
      draft = null;
      if (valid) begin(value);
      control.observe();
    });
    for (const event of ['pointercancel', 'blur']) progress.addEventListener(event, () => {
      draft = null; control.observe();
    });
    $('seek-range').addEventListener('input', () => control.choose(Number($('seek-range').value)));
    $('seek-back').addEventListener('click', () => control.step(-15));
    $('seek-forward').addEventListener('click', () => control.step(15));
    $('seek-form').addEventListener('submit', event => { event.preventDefault(); void control.confirm(); });
    $('seek-cancel').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => {
      if (control.state.open) control.close();
      if (opener?.isConnected && !opener.disabled && !opener.closest('[inert]')) opener.focus({ preventScroll: true });
      opener = null;
    });
    repaint(control.state);
    return control;
  }
  return { observation, model, create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardSeek;
