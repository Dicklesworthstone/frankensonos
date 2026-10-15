// Playback clocks use observed state and a local monotonic clock, not a poll loop.
'use strict';
const PlaybackRemote = (() => {
  const MAX_SECONDS = 0xffffffff;
  const text = (value) => typeof value === 'string' && value.trim().length > 0;
  const seconds = (value) => Number.isInteger(value) && value >= 0 && value <= MAX_SECONDS;
  const optional = (value) => value == null || seconds(value);

  function shape(zone) {
    if (!zone || !text(zone.household) || !text(zone.coordinator_room)
        || !Array.isArray(zone.members) || !zone.members.length
        || !zone.members.every(text) || !zone.members.includes(zone.coordinator_room)
        || new Set(zone.members).size !== zone.members.length) {
      throw new Error('The playback group is incomplete. Refresh the rooms.');
    }
    return JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]);
  }

  /** Copy only the timing and identity fields of the public ZoneStateDto. */
  function decode(value, group) {
    if (!value || shape(value.zone) !== group
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)) {
      throw new Error('The playback group or state changed. Refresh the rooms.');
    }
    const track = value.track;
    if (track == null) return Object.freeze({ transport: value.transport_state, track: null, identity: null });
    if (!text(track.uri) || track.uri === 'NOT_IMPLEMENTED'
        || !optional(track.position_secs) || !optional(track.duration_secs)
        || !optional(track.queue_position) || track.queue_position === 0
        || (track.duration_secs > 0 && track.position_secs > track.duration_secs)) {
      throw new Error('The track position is unreadable. Refresh its state.');
    }
    const timing = Object.freeze({
      uri: track.uri, position: track.position_secs ?? null,
      duration: track.duration_secs > 0 ? track.duration_secs : null,
      slot: track.queue_position ?? null,
    });
    return Object.freeze({ transport: value.transport_state, track: timing,
      identity: JSON.stringify([group, timing.uri, timing.slot, timing.duration, value.transport_state]) });
  }

  function time(value) {
    if (!seconds(value)) return '–:––';
    const hours = Math.floor(value / 3600), minutes = Math.floor(value / 60) % 60;
    return `${hours ? `${hours}:${String(minutes).padStart(2, '0')}` : minutes}:${String(value % 60).padStart(2, '0')}`;
  }

  /** Seconds or [hours:]minutes:seconds, without fractional or negative time. */
  function parseTime(value) {
    if (typeof value !== 'string' || value.length > 32) return null;
    const parts = value.trim().split(':');
    if (parts.length > 3 || !/^[0-9]{1,10}$/.test(parts[0])
        || parts.slice(1).some((part) => !/^[0-5][0-9]$/.test(part))) return null;
    const total = parts.reduce((sum, part) => sum * 60 + Number(part), 0);
    return seconds(total) ? total : null;
  }

  function position(sample, elapsed) {
    const track = sample?.track;
    if (track?.position == null) return null;
    const advance = sample.transport === 'playing' && Number.isFinite(elapsed) ? Math.max(0, elapsed) / 1000 : 0;
    return Math.min(track.duration ?? MAX_SECONDS, Math.floor(track.position + advance));
  }

  function canSeek(sample) {
    return !!sample?.track && sample.track.position !== null && sample.track.duration !== null
      && ['playing', 'paused'].includes(sample.transport);
  }

  function acknowledgement(value) {
    return value && typeof value.done === 'string' && typeof value.changed === 'boolean'
      && (value.notes == null || (Array.isArray(value.notes)
        && value.notes.every((note) => note && typeof note.detail === 'string')));
  }

  function model({ zone, call, changed = () => {}, current = () => true,
    execute = (work) => work(), blocked = () => false, externalRefresh = false,
    now = () => performance.now(), schedule = setTimeout, cancel = clearTimeout, lanes = new Map() }) {
    const group = shape(zone), name = `${zone.coordinator_room}@${zone.household}`;
    const owner = {};
    const lane = lanes.get(name) || { owner: null, uncertain: false, result: null, listeners: new Set() };
    lanes.set(name, lane);
    let sample = null, anchor = 0, frozen = null, fresh = false, active = true;
    let disposed = false, reading = false, serial = 0, timer = null;
    let message = 'Waiting for playback position.';
    let review = null, pending = false, sent = false, uncertain = lane.uncertain, result = lane.result;
    const forgetLane = () => { if (lanes.get(name) === lane && !lane.owner && !lane.uncertain && !lane.listeners.size) lanes.delete(name); };
    function laneChanged() {
      uncertain = lane.uncertain;
      if (lane.result) result = lane.result;
      stale(lane.owner ? 'Another seek is pending in this group.'
        : 'A seek in this group finished. Refresh position before choosing again.');
    }
    lane.listeners.add(laneChanged);
    const notifyLane = () => { for (const listener of lane.listeners) if (listener !== laneChanged) listener(); };
    let epoch = 0, lifetime = 0;
    const live = () => !disposed && active && current();
    const stop = () => { if (timer !== null) cancel(timer); timer = null; };
    const at = () => fresh && live() ? position(sample, now() - anchor) : frozen;
    const writable = () => live() && fresh && !reading && !pending && !uncertain
      && !lane.owner && !blocked() && canSeek(sample);
    function view() {
      const elapsed = at(), duration = sample?.track?.duration ?? null;
      return Object.freeze({ elapsed, duration,
        remaining: duration !== null && elapsed !== null ? duration - elapsed : null,
        transport: sample?.transport ?? 'unknown', fresh: fresh && live(), reading,
        seekable: writable(), review, pending: pending || !!lane.owner, uncertain, result,
        trackIdentity: sample?.identity ?? null,
        estimated: fresh && live() && elapsed !== null && sample?.transport === 'playing', message });
    }
    function emit() {
      if (disposed || !active || !current()) return;
      try { changed(view()); } catch (_) { /* A view cannot change the operation's result. */ }
    }
    function tick() {
      stop();
      if (!live() || !fresh || sample?.transport !== 'playing' || at() === null) return;
      // Reaching a predicted end is not a track transition or proof of stopping.
      if (sample.track.duration !== null && at() >= sample.track.duration) return;
      timer = schedule(() => { timer = null; emit(); tick(); }, 1000);
    }
    function stale(detail = 'Position is stale; refresh to read it again.') {
      frozen = at(); fresh = false; reading = false; ++serial;
      review = null; ++epoch;
      message = detail; stop(); emit();
    }
    function accept(value) {
      const next = decode(value, group);
      if (next.identity !== sample?.identity || !canSeek(next)) { review = null; ++epoch; }
      sample = next; anchor = now(); frozen = position(sample, 0); fresh = true;
      message = !sample.track ? 'Nothing playing.' : sample.track.position === null ? 'Position unavailable.'
        : sample.track.duration === null ? 'Duration unavailable; elapsed time only.' : '';
      emit(); tick();
    }
    function observe(value) {
      if (!live() || sent) return false;
      ++serial; reading = false;
      try { accept(value); return true; }
      catch (error) { stale(error.message); return false; }
    }
    async function refresh() {
      if (!live() || reading || pending || lane.owner || blocked()) return false;
      stale('Reading playback position…'); const version = serial;
      reading = true; emit();
      try {
        const value = await call('state', { params: { room: name } });
        if (!live() || serial !== version) return false;
        // Only a successful explicit read clears an uncertain write. Passive
        // updates may display current timing but cannot silently allow a retry.
        decode(value, group);
        uncertain = lane.uncertain = false; lane.result = null; notifyLane();
        reading = false; accept(value); return true;
      } catch (error) {
        if (live() && serial === version) stale(error.hint || error.message || 'Position could not be read.');
        return false;
      } finally {
        if (serial === version) { reading = false; emit(); }
      }
    }
    function preview(target) {
      if (!live() || pending) return null;
      review = null; ++epoch;
      if (!writable() || !seconds(target) || target >= sample.track.duration) { emit(); return null; }
      result = null;
      review = Object.freeze({ target, identity: sample.identity });
      emit();
      return review;
    }
    function nudge(delta) {
      if (!Number.isInteger(delta) || ![-15, 15].includes(delta) || !writable()) return null;
      return preview(Math.max(0, Math.min(sample.track.duration - 1, at() + delta)));
    }
    function cancelReview() {
      if (!live() || sent) return;
      review = null; ++epoch; emit();
    }
    async function confirm(selected) {
      if (!selected || selected !== review || !writable()) return false;
      const version = epoch, life = lifetime;
      const valid = () => live() && version === epoch && life === lifetime;
      let entered = false, acknowledged = false;
      review = null; pending = true; result = null;
      lane.owner = owner; lane.result = null; notifyLane(); emit();
      try {
        // The card's native action lane owns both preflight and dispatch, so
        // play/next/previous cannot interleave with a reviewed seek.
        await execute(async () => {
          entered = true;
          try {
            if (!valid()) return;
            const observed = decode(await call('state', { params: { room: name } }), group);
            if (!valid()) return;
            if (observed.identity !== selected.identity || !canSeek(observed)
                || selected.target >= observed.track.duration) {
              throw new Error('The track or playback state changed. Refresh and choose a new position.');
            }
            frozen = at(); fresh = false; ++serial; stop();
            message = 'Sending the reviewed position…'; emit();
            if (!valid()) return;
            sent = true;
            // This is a client preflight, not an atomic speaker reservation.
            // Keep the native contract: no guessed fields or resume command.
            const outcome = await call('seek', { body: { zone: name, to_secs: selected.target } });
            if (!acknowledgement(outcome)) throw new Error('The seek reply is incomplete.');
            acknowledged = true;
            lane.result = Object.freeze({ status: 'accepted', detail: outcome.done });
            if (!disposed) result = lane.result;
            if (live() && life === lifetime) {
              stale('Seek accepted; waiting for an observed playback position.');
            }
            return outcome;
          } catch (error) {
            // A sent write can finish even after closing this view. Retain its
            // uncertainty across hide/show without dispatching or replaying it.
            if (sent) uncertain = lane.uncertain = true;
            const detail = sent
              ? 'Seek result uncertain. Refresh position before making another selection; the seek was not repeated.'
              : 'The seek was not sent. Refresh position and review it again.';
            // Keep the remedy for a sent action even if its reply arrives in
            // a hidden view. Resuming must not show unexplained disabled controls.
            const failure = Object.freeze({ status: sent ? 'uncertain' : 'not_sent',
              detail: [detail, error?.hint || error?.message].filter(Boolean).join(' ') });
            if (sent) lane.result = failure;
            if (!disposed && (sent || (live() && life === lifetime))) result = failure;
            if (live() && life === lifetime) stale(detail);
            throw error;
          } finally {
            pending = false; sent = false; emit();
          }
        });
      } catch (_) { /* The outcome above survives a card's error presenter. */ }
      finally {
        pending = false; sent = false;
        if (lane.owner === owner) lane.owner = null;
        notifyLane(); forgetLane();
        if (live() && life === lifetime) {
          if (!entered) { review = null; message = 'Another control is busy. Choose the position again.'; }
          emit();
          // Embedded cards already schedule their shared refresh after an
          // action. Standalone controllers refresh only acknowledged writes.
          if (!externalRefresh && acknowledged) await refresh();
        }
      }
      return acknowledged;
    }
    return { view, observe, refresh, stale, preview, nudge, confirm, cancelReview,
      block() { emit(); },
      suspend() { if (!disposed) { ++lifetime; stale('Position paused while this page is hidden.'); active = false; stop(); } },
      resume() { if (!disposed) { ++lifetime; active = true; stale(); } },
      dispose() {
        if (disposed) return;
        disposed = true; active = false; ++lifetime; ++epoch; ++serial; review = null; stop();
        lane.listeners.delete(laneChanged); forgetLane();
      },
    };
  }

  function panel({ zone, call, el, current, now, schedule, cancel, execute, blocked, externalRefresh, lanes }) {
    const elapsed = el('span', { text: '–:––' });
    const remaining = el('span');
    const progress = el('progress', { max: 1, value: 0, hidden: true,
      'aria-label': `Playback position in ${zone.members.join(' + ')}` });
    const detail = el('div', { class: 'playback-detail' });
    const outcome = el('div', { class: 'playback-detail', role: 'status' });
    const refresh = el('button', { type: 'button', text: 'Refresh position' });
    const range = el('input', { type: 'range', min: 0, max: 1, value: 0, step: 1, disabled: true,
      'aria-label': `Choose a playback position in ${zone.members.join(' + ')}` });
    const exact = el('input', { type: 'text', maxlength: 32, placeholder: 'm:ss or h:mm:ss', disabled: true,
      'aria-label': `Exact playback time in ${zone.members.join(' + ')}`,
      title: 'Seconds, m:ss or h:mm:ss. Enter confirms the reviewed time; Escape cancels.' });
    const guidance = el('div', { class: 'playback-detail' });
    const restart = el('button', { type: 'button', text: 'Restart', disabled: true });
    const back = el('button', { type: 'button', text: '−15 seconds', disabled: true });
    const ahead = el('button', { type: 'button', text: '+15 seconds', disabled: true });
    const consent = el('div', { class: 'row', hidden: true });
    let shown = null, apply = null, cancelButton = null, editing = false, lastIdentity = null;
    const node = el('section', { class: 'playback-clock', 'aria-label': 'Playback position' },
      el('div', { class: 'playback-time' }, elapsed, remaining), progress, detail, range, exact, guidance,
      el('div', { class: 'row' }, restart, back, ahead, refresh), consent, outcome);
    const controller = model({ zone, call, current, now, schedule, cancel, execute, blocked, externalRefresh, lanes, changed(state) {
      if (state.trackIdentity !== lastIdentity || !state.fresh || state.pending) {
        editing = false;
        if (guidance.textContent) guidance.textContent = '';
      }
      lastIdentity = state.trackIdentity;
      if (!editing) exact.value = state.review ? time(state.review.target) : '';
      elapsed.textContent = `${state.estimated ? '≈ ' : ''}${time(state.elapsed)} / ${time(state.duration)}`;
      remaining.textContent = state.remaining === null ? '' : `${time(state.remaining)} left`;
      progress.hidden = state.duration === null || state.elapsed === null;
      progress.max = state.duration ?? 1; progress.value = state.elapsed ?? 0;
      detail.textContent = state.message || (state.estimated ? 'Estimated from the latest playback sample.' : 'Observed playback position.');
      const resultText = state.result?.detail || '';
      if (outcome.textContent !== resultText) outcome.textContent = resultText;
      refresh.disabled = state.reading || state.pending || !!blocked?.();
      for (const control of [range, exact, restart, back, ahead]) control.disabled = !state.seekable;
      range.max = Math.max(0, (state.duration ?? 1) - 1);
      range.value = String(state.review?.target ?? Math.min(Number(range.max), state.elapsed ?? 0));
      range.setAttribute('aria-valuetext', time(Number(range.value)));
      if (shown !== state.review) {
        shown = state.review;
        consent.replaceChildren(); consent.hidden = !shown;
        if (shown) {
          const selected = shown;
          apply = el('button', { type: 'button', text: `Seek to ${time(selected.target)}` });
          cancelButton = el('button', { type: 'button', text: 'Cancel seek' });
          apply.addEventListener('click', () => { void controller.confirm(selected); });
          cancelButton.addEventListener('click', () => { editing = false; controller.cancelReview(); });
          consent.append(apply, cancelButton);
        }
      }
      if (apply) apply.disabled = !state.seekable || !state.review;
    } });
    exact.addEventListener('input', () => {
      editing = true;
      const reviewed = controller.preview(parseTime(exact.value));
      guidance.textContent = reviewed ? 'Time previewed; confirm below or press Enter to seek.'
        : `Enter a time below ${time(controller.view().duration)} (seconds, m:ss or h:mm:ss).`;
    });
    exact.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault(); editing = false;
        void controller.confirm(controller.view().review);
      } else if (event.key === 'Escape') {
        event.preventDefault(); editing = false; guidance.textContent = ''; controller.cancelReview();
      }
    });
    range.addEventListener('input', () => { editing = false; guidance.textContent = ''; controller.preview(Number(range.value)); });
    restart.addEventListener('click', () => { editing = false; controller.preview(0); });
    back.addEventListener('click', () => { editing = false; controller.nudge(-15); });
    ahead.addEventListener('click', () => { editing = false; controller.nudge(15); });
    refresh.addEventListener('click', () => { void controller.refresh(); });
    return { node, ...controller };
  }

  return { model, panel, decode, shape, position, time, parseTime, MAX_SECONDS };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = PlaybackRemote;
if (typeof registerCardPanel === 'function') {
  // This remote retains pending seeks and unresolved results across card replacement.
  const seekLanes = new Map();
  ROUTES.seek = ['POST', '/seek'];
  registerCardPanel((parts) => {
    const panel = PlaybackRemote.panel({ zone: parts.zone, call, el, current: () => ownsCard(parts),
      execute: (work) => act(parts, work), blocked: () => parts.writing || transportBusy(parts), externalRefresh: true, lanes: seekLanes });
    const visibility = () => { if (document.hidden) panel.suspend(); else panel.resume(); };
    const hide = () => panel.suspend(), show = () => { if (!document.hidden) panel.resume(); };
    document.addEventListener('visibilitychange', visibility);
    window.addEventListener('pagehide', hide);
    window.addEventListener('pageshow', show);
    if (document.hidden) panel.suspend();
    const dispose = panel.dispose;
    panel.dispose = () => {
      document.removeEventListener('visibilitychange', visibility);
      window.removeEventListener('pagehide', hide);
      window.removeEventListener('pageshow', show);
      dispose();
    };
    return panel;
  });
}
