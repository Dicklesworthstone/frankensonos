// Independent room levels use the dashboard's shared native command lane.
'use strict';
const DashboardVolume = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const MAX_AGE = 45000;
  const level = value => Number.isInteger(value) && value >= 0 && value <= 100;
  const address = (room, zone) => room + '@' + zone.household;
  const fail = message => { throw new Error(message); };
  function levelOf(value) {
    return typeof value === 'string' && /^\d{1,3}$/.test(value.trim()) && level(Number(value))
      ? Number(value) : null;
  }
  function observed(value, zone, name) {
    const playback = C.playback(value, zone);
    if (!zone.members.includes(name)) fail('This room no longer belongs to the selected group.');
    return Object.freeze({ target: address(name, zone), name, volume: playback.volume ?? null, mute: playback.mute ?? null });
  }
  function groupOf(value, target, scope) {
    const zone = C.zones(value).find(zone => C.key(zone) === target);
    if (!zone || C.shape(zone) !== scope) fail('The group or coordinator changed. Refresh playback and read room levels again.');
    return zone;
  }
  function confirmed(value, kind) {
    if (!value || typeof value.done !== 'string' || !value.done.trim() || typeof value.changed !== 'boolean'
        || (kind === 'volume' && !level(value.volume))) fail('The room-level change was not confirmed.');
    // A policy-clamped volume is a successful native result, not a reason to retry.
    return value;
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const choices = new Map();
    const state = { open: false, target: '', scope: '', label: '', rows: null, stale: true,
      loading: false, writing: false, pending: null, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => controller.selected();
    const live = () => !disposed && active && state.open;
    const matching = () => selected() && state.target === C.key(selected().zone) && state.scope === C.shape(selected().zone);
    const row = target => state.rows?.find(value => value.target === target);
    const canRead = () => !!(live() && selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.rows && !state.stale && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    const desired = target => choices.get(target) ?? row(target)?.volume ?? null;
    function scope() {
      const item = selected();
      state.target = item ? C.key(item.zone) : '';
      state.scope = item ? C.shape(item.zone) : '';
      state.label = item ? item.zone.members.join(' + ') + ' · ' + item.zone.household : 'Choose a listening group';
    }
    function retire(message, discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false;
      state.pending = null; state.stale = true; choices.clear();
      if (discard) { state.rows = null; readAt = 0; }
      if (message) state.message = message;
    }
    function observe() {
      if (disposed) return;
      if (!matching()) { retire('The selected group changed. Read its room levels again.', true); scope(); }
      else if ((state.loading || state.pending || (state.rows && !state.stale))
          && (!selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (state.rows && !state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read current room levels before making another change. Refresh playback first if the group is stale.');
      }
      publish();
    }
    async function read(keepResult = false) {
      if (!canRead()) return false;
      retire(); scope();
      const v = version, target = state.target, shape = state.scope, abort = new AbortController();
      reading = abort; state.loading = true;
      if (!keepResult) state.result = null;
      state.message = state.result ? state.result.done + ' Reading room levels…' : 'Reading room levels…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh) fail('The room-level observation is no longer current.');
      };
      try {
        const options = { signal: abort.signal };
        const zone = groupOf(await call('/zones', undefined, options), target, shape); valid();
        const results = new Map(), stamps = [];
        await C.each(zone.members, async name => {
          valid();
          try {
            const value = observed(await call('/zones/' + encodeURIComponent(address(name, zone)) + '/state', undefined, options), zone, name);
            valid(); stamps.push(now()); results.set(name, Object.freeze({ ...value, error: '' }));
          } catch (error) {
            valid();
            results.set(name, Object.freeze({ target: address(name, zone), name, volume: null, mute: null, error: C.problem(error) }));
          }
        }, 4);
        valid(); groupOf(await call('/zones', undefined, options), target, shape); valid();
        state.rows = Object.freeze(zone.members.map(name => results.get(name))); state.stale = false;
        readAt = stamps.length ? Math.min(...stamps) : now();
        state.message = state.result?.done || ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = [state.result?.done, C.problem(error)].filter(Boolean).join(' ');
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function choose(target, value) {
      if (!editable() || !level(row(target)?.volume)) return false;
      const next = levelOf(value);
      ++version; state.pending = null; state.result = null;
      if (next === null) choices.delete(target); else choices.set(target, next);
      state.message = next === null ? 'Choose a whole room level from 0 to 100.' : '';
      publish(); return next !== null;
    }
    function canReview(target, kind) {
      const value = row(target);
      if (!editable() || !value) return false;
      if (kind === 'volume') return level(value.volume) && level(desired(target)) && desired(target) !== value.volume;
      return kind === 'mute' && typeof value.mute === 'boolean';
    }
    function review(target, kind) {
      if (!canReview(target, kind)) return null;
      const current = row(target), want = kind === 'volume' ? desired(target) : !current.mute;
      const title = kind === 'volume' ? `${current.name}: ${current.volume} → ${want}`
        : `${want ? 'Mute' : 'Unmute'} ${current.name}`;
      const detail = kind === 'volume' ? 'Adjust this room’s volume. House volume limits and quiet hours still apply.'
        : want ? 'Silence this room while the group keeps its current playback.'
          : `Unmute this room at ${current.volume === null ? 'its existing volume' : 'its observed volume ' + current.volume}. House policy still applies.`;
      state.pending = Object.freeze({ version, target: state.target, scope: state.scope, label: state.label,
        room: current, kind, want, title, detail,
        body: Object.freeze({ zone: current.target, ...(kind === 'volume' ? { volume: want } : { mute: want }) }) });
      state.message = ''; state.result = null; publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version && row(state.pending.room.target) === state.pending.room);
    async function apply(pending = state.pending) {
      if (!canApply() || pending !== state.pending) return false;
      const v = version;
      let outcome = null, ok = false;
      state.pending = null; state.writing = true; state.result = null; state.message = 'Checking the room and its current level…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || !matching() || target !== pending.target || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('The room-level review is no longer current. Read levels and review again.');
          };
          valid();
          const zone = groupOf(await call('/zones'), target, pending.scope); valid();
          const current = observed(await call('/zones/' + encodeURIComponent(pending.room.target) + '/state'), zone, pending.room.name); valid();
          groupOf(await call('/zones'), target, pending.scope); valid();
          if (current.volume !== pending.room.volume || current.mute !== pending.room.mute) {
            fail('The room volume or mute changed. Read its level and review again.');
          }
          const result = confirmed(await send('/' + pending.kind, pending.body), pending.kind);
          outcome = Object.freeze({ target: pending.room.target, name: pending.room.name, kind: pending.kind,
            requested: pending.want, volume: pending.kind === 'volume' ? result.volume : null });
          return result;
        }, { wholeHouse: false });
        if (ok) { state.result = Object.freeze({ ...outcome, done: controller.state.message }); state.message = state.result.done; }
        else state.message = controller.state.message || 'No room change was sent. Refresh playback and read the levels again.';
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok && state.target === pending.target && state.scope === pending.scope && canRead()) await read(true);
      return ok;
    }
    return { state, read, canRead, editable, desired, choose, canReview, review, canApply, apply, observe,
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
    const $ = id => doc.getElementById(id), dialog = $('room-volume-dialog'), trigger = $('open-room-volume'), container = $('room-volume-rooms');
    let opener = null, shown = null, reviewFocus = null, signature = '';
    const nodes = new Map();
    const control = model({ call, controller, now, changed: render });
    const element = (tag, className, text) => {
      const node = doc.createElement(tag); node.className = className || '';
      if (text !== undefined) node.textContent = text;
      return node;
    };
    function createRow(value) {
      const node = element('article', 'volume-room'), header = element('header'), name = element('h3', '', value.name);
      const status = element('p', 'volume-room-status'), range = element('input'), output = element('output', 'volume-room-value');
      range.type = 'range'; range.min = '0'; range.max = '100'; range.step = '1'; range.disabled = true;
      range.setAttribute('aria-label', value.target + ' volume');
      const slider = element('div', 'volume-room-slider'), actions = element('div', 'volume-room-actions');
      const apply = element('button', 'outline-button', 'Review level'), mute = element('button', 'text-button', 'Mute');
      apply.type = mute.type = 'button';
      range.addEventListener('input', event => control.choose(value.target, event.target.value));
      apply.addEventListener('click', () => control.review(value.target, 'volume'));
      mute.addEventListener('click', () => control.review(value.target, 'mute'));
      header.append(name, output); slider.append(range); actions.append(apply, mute); node.append(header, status, slider, actions);
      return { node, status, range, output, apply, mute };
    }
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('room-volume-target').textContent = s.label; $('room-volume-read').disabled = !control.canRead();
      const next = JSON.stringify(s.rows?.map(value => value.target) || []);
      if (next !== signature) {
        signature = next; nodes.clear(); container.replaceChildren();
        for (const value of s.rows || []) { const parts = createRow(value); nodes.set(value.target, parts); container.append(parts.node); }
        if (!s.rows) container.append(element('p', 'small-note', 'Read the selected group’s room levels to begin.'));
      }
      for (const value of s.rows || []) {
        const parts = nodes.get(value.target), wanted = control.desired(value.target);
        parts.range.disabled = !control.editable() || !level(value.volume);
        if (parts.range.value !== String(wanted ?? 0)) parts.range.value = String(wanted ?? 0);
        parts.output.textContent = wanted === null ? '—' : String(wanted) + (wanted !== value.volume ? ' · not applied' : '');
        parts.status.textContent = value.error || `${s.stale ? 'Last observed · ' : ''}${value.volume === null ? 'Volume unavailable' : 'Volume ' + value.volume} · ${value.mute === null ? 'Mute unavailable' : value.mute ? 'Muted' : 'Unmuted'}`;
        parts.apply.disabled = !control.canReview(value.target, 'volume');
        parts.mute.disabled = !control.canReview(value.target, 'mute');
        parts.mute.textContent = value.mute === true ? 'Unmute' : 'Mute';
        parts.mute.setAttribute('aria-pressed', String(value.mute === true));
        parts.mute.setAttribute('aria-label', parts.mute.textContent + ' ' + value.target);
      }
      const preview = $('room-volume-preview'); preview.hidden = !s.pending;
      $('room-volume-preview-title').textContent = s.pending?.title || '';
      $('room-volume-preview-detail').textContent = s.pending ? s.pending.detail + ' Group: ' + s.pending.label + '.' : '';
      $('room-volume-confirm').disabled = !control.canApply();
      $('room-volume-confirm').textContent = s.writing ? 'Applying…' : 'Confirm room change';
      $('room-volume-result').textContent = s.result?.kind === 'volume'
        ? `${s.result.name}: accepted volume ${s.result.volume}${s.result.volume !== s.result.requested ? ' (requested ' + s.result.requested + ')' : ''}.` : '';
      $('room-volume-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('room-volume-read').addEventListener('click', () => { void control.read(); });
    $('room-volume-back').addEventListener('click', () => control.cancelReview());
    $('room-volume-confirm').addEventListener('click', () => { void control.apply(); });
    $('close-room-volume').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return control;
  }
  return { MAX_AGE, levelOf, observed, confirmed, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardVolume;
