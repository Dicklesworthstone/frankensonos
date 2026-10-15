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

  function position(sample, elapsed) {
    const track = sample?.track;
    if (track?.position == null) return null;
    const advance = sample.transport === 'playing' && Number.isFinite(elapsed) ? Math.max(0, elapsed) / 1000 : 0;
    return Math.min(track.duration ?? MAX_SECONDS, Math.floor(track.position + advance));
  }

  function model({ zone, call, changed = () => {}, current = () => true,
    now = () => performance.now(), schedule = setTimeout, cancel = clearTimeout }) {
    const group = shape(zone), name = `${zone.coordinator_room}@${zone.household}`;
    let sample = null, anchor = 0, frozen = null, fresh = false, active = true;
    let disposed = false, reading = false, serial = 0, timer = null;
    let message = 'Waiting for playback position.';
    const live = () => !disposed && active && current();
    const stop = () => { if (timer !== null) cancel(timer); timer = null; };
    const at = () => fresh && live() ? position(sample, now() - anchor) : frozen;
    function view() {
      const elapsed = at(), duration = sample?.track?.duration ?? null;
      return Object.freeze({ elapsed, duration,
        remaining: duration !== null && elapsed !== null ? duration - elapsed : null,
        transport: sample?.transport ?? 'unknown', fresh: fresh && live(), reading,
        estimated: fresh && live() && elapsed !== null && sample?.transport === 'playing', message });
    }
    function emit() {
      if (disposed || !current()) return;
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
      message = detail; stop(); emit();
    }
    function accept(value) {
      sample = decode(value, group); anchor = now(); frozen = position(sample, 0); fresh = true;
      message = !sample.track ? 'Nothing playing.' : sample.track.position === null ? 'Position unavailable.'
        : sample.track.duration === null ? 'Duration unavailable; elapsed time only.' : '';
      emit(); tick();
    }
    function observe(value) {
      if (!live()) return false;
      ++serial; reading = false;
      try { accept(value); return true; }
      catch (error) { stale(error.message); return false; }
    }
    async function refresh() {
      if (!live() || reading) return false;
      stale('Reading playback position…'); const version = serial;
      reading = true; emit();
      try {
        const value = await call('state', { params: { room: name } });
        if (!live() || serial !== version) return false;
        reading = false; accept(value); return true;
      } catch (error) {
        if (live() && serial === version) stale(error.hint || error.message || 'Position could not be read.');
        return false;
      } finally {
        if (serial === version) { reading = false; emit(); }
      }
    }
    return { view, observe, refresh, stale,
      suspend() { if (!disposed) { stale('Position paused while this page is hidden.'); active = false; stop(); } },
      resume() { if (!disposed) { active = true; stale(); } },
      dispose() { disposed = true; active = false; ++serial; stop(); },
    };
  }

  function panel({ zone, call, el, current, now, schedule, cancel }) {
    const elapsed = el('span', { text: '–:––' });
    const remaining = el('span');
    const progress = el('progress', { max: 1, value: 0, hidden: true,
      'aria-label': `Playback position in ${zone.members.join(' + ')}` });
    const detail = el('div', { class: 'playback-detail' });
    const refresh = el('button', { type: 'button', text: 'Refresh position' });
    const node = el('section', { class: 'playback-clock', 'aria-label': 'Playback position' },
      el('div', { class: 'playback-time' }, elapsed, remaining), progress, detail, refresh);
    const controller = model({ zone, call, current, now, schedule, cancel, changed(state) {
      elapsed.textContent = `${state.estimated ? '≈ ' : ''}${time(state.elapsed)} / ${time(state.duration)}`;
      remaining.textContent = state.remaining === null ? '' : `${time(state.remaining)} left`;
      progress.hidden = state.duration === null || state.elapsed === null;
      progress.max = state.duration ?? 1; progress.value = state.elapsed ?? 0;
      detail.textContent = state.message || (state.estimated ? 'Estimated from the latest playback sample.' : 'Observed playback position.');
      refresh.disabled = state.reading;
    } });
    refresh.addEventListener('click', () => { void controller.refresh(); });
    return { node, ...controller };
  }

  return { model, panel, decode, shape, position, time, MAX_SECONDS };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = PlaybackRemote;
if (typeof registerCardPanel === 'function') {
  registerCardPanel((parts) => {
    const panel = PlaybackRemote.panel({ zone: parts.zone, call, el, current: () => ownsCard(parts) });
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
