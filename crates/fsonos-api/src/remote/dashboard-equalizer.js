// Per-room tone uses the existing equalizer API and the shared command lane.
'use strict';
const DashboardEqualizer = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const MAX_AGE = 45000, FIELDS = ['bass', 'treble', 'loudness'];
  const level = value => Number.isInteger(value) && value >= -10 && value <= 10;
  const tone = value => ({ bass: value.bass, treble: value.treble, loudness: value.loudness });
  const same = (a, b) => FIELDS.every(field => a[field] === b[field]);
  const signed = value => value > 0 ? '+' + value : String(value);
  const describe = value => `bass ${signed(value.bass)}, treble ${signed(value.treble)}, loudness ${value.loudness ? 'on' : 'off'}`;
  const fail = message => { throw new Error(message); };
  function observed(value, room) {
    if (!value || value.room !== room.name || !level(value.bass) || !level(value.treble)
        || typeof value.loudness !== 'boolean') fail('The speaker sound response is incomplete or outside the supported range. Read it again.');
    return Object.freeze({ target: room.target, name: room.name, ...tone(value) });
  }
  function groupOf(value, target, scope) {
    const zone = C.zones(value).find(zone => C.key(zone) === target);
    if (!zone || C.shape(zone) !== scope) fail('The group or coordinator changed. Refresh playback and read speaker sound again.');
    return zone;
  }
  function confirmed(value, pending) {
    const eq = observed(value?.eq, pending.room);
    if (typeof value?.done !== 'string' || !value.done.trim() || !same(eq, pending.wanted)) {
      fail('The requested speaker sound was not confirmed. Read current sound before another change.');
    }
    // EqSetDto carries `done` and `eq`; the command lane accepts OutcomeDto.
    return { done: value.done, changed: true, eq };
  }

  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let version = 0, active = true, disposed = false, reading = null, readAt = 0;
    const choices = new Map();
    const state = { open: false, target: '', scope: '', label: '', rows: null, stale: true,
      loading: false, writing: false, pending: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => controller.selected();
    const live = () => !disposed && active && state.open;
    const matching = () => selected() && state.target === C.key(selected().zone) && state.scope === C.shape(selected().zone);
    const row = target => state.rows?.find(value => value.target === target);
    const canRead = () => !!(live() && selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && matching() && state.rows && !state.stale && controller.state.topologyFresh
      && !state.loading && !state.writing && now() - readAt < MAX_AGE && controller.canControl());
    const desired = target => choices.get(target) ?? (row(target)?.error === '' ? tone(row(target)) : null);
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
      if (!matching()) { retire('The selected group changed. Read its speaker sound again.', true); scope(); }
      else if ((state.loading || state.pending || (state.rows && !state.stale))
          && (!selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (state.rows && !state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read current speaker sound before editing again. Refresh playback first if the group is stale.');
      }
      publish();
    }
    async function read(keepResult = false) {
      if (!canRead()) return false;
      retire(); scope();
      const v = version, target = state.target, shape = state.scope, abort = new AbortController();
      reading = abort; state.loading = true;
      if (!keepResult) state.result = null;
      state.message = state.result ? state.result.done + ' Reading current sound…' : 'Reading speaker sound…'; publish();
      const valid = () => {
        if (!live() || v !== version || !matching() || !selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh) fail('The sound observation is no longer current.');
      };
      try {
        const options = { signal: abort.signal };
        const zone = groupOf(await call('/zones', undefined, options), target, shape); valid();
        const results = new Map(), stamps = [];
        await C.each(zone.members, async name => {
          valid();
          const room = { target: name + '@' + zone.household, name };
          try {
            const value = observed(await call('/zones/' + encodeURIComponent(room.target) + '/eq', undefined, options), room);
            valid(); stamps.push(now()); results.set(name, Object.freeze({ ...value, error: '' }));
          } catch (error) {
            valid(); results.set(name, Object.freeze({ ...room, error: C.problem(error) }));
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
    function choose(target, field, value) {
      const current = desired(target);
      if (!editable() || !current || !FIELDS.includes(field)
          || (field === 'loudness' ? typeof value !== 'boolean' : !level(value))) return false;
      ++version; state.pending = null; state.result = null;
      choices.set(target, { ...current, [field]: value }); state.message = ''; publish(); return true;
    }
    function reset(target, flat = false) {
      if (!editable() || !desired(target)) return false;
      ++version; state.pending = null; state.result = null;
      if (flat) choices.set(target, { bass: 0, treble: 0, loudness: false }); else choices.delete(target);
      state.message = ''; publish(); return true;
    }
    const canReview = target => !!(editable() && desired(target) && !same(row(target), desired(target)));
    function review(target) {
      if (!canReview(target)) return null;
      const current = row(target), wanted = Object.freeze(tone(desired(target))), body = { zone: target };
      for (const field of FIELDS) if (current[field] !== wanted[field]) body[field] = wanted[field];
      state.pending = Object.freeze({ version, target: state.target, scope: state.scope, label: state.label,
        room: current, wanted, body: Object.freeze(body) });
      state.message = ''; state.result = null; publish(); return state.pending;
    }
    const canApply = () => !!(editable() && state.pending && state.pending.version === version && row(state.pending.room.target) === state.pending.room);
    async function apply(pending = state.pending) {
      if (!canApply() || pending !== state.pending) return false;
      const v = version;
      let receipt = null, ok = false;
      state.pending = null; state.writing = true; state.result = null; state.message = 'Checking this room and its current sound…'; publish();
      try {
        ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || !matching() || target !== pending.target || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('The sound review is no longer current. Read sound and review again.');
          };
          valid(); groupOf(await call('/zones'), target, pending.scope); valid();
          const current = observed(await call('/zones/' + encodeURIComponent(pending.room.target) + '/eq'), pending.room); valid();
          groupOf(await call('/zones'), target, pending.scope); valid();
          if (!same(current, pending.room)) fail('The speaker sound changed. Read its current settings and review again.');
          receipt = confirmed(await send('/eq', pending.body), pending);
          return receipt;
        }, { wholeHouse: false });
        if (ok) {
          state.result = Object.freeze({ target: pending.room.target, name: pending.room.name,
            eq: receipt.eq, done: controller.state.message });
          state.message = state.result.done;
        } else state.message = controller.state.message || 'No sound change was sent. Refresh playback and read speaker sound again.';
      } finally {
        state.writing = false; retire(); publish();
      }
      if (ok && state.target === pending.target && state.scope === pending.scope && canRead()) await read(true);
      return ok;
    }
    return { state, read, canRead, editable, desired, choose, reset, canReview, review, canApply, apply, observe,
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
    const $ = id => doc.getElementById(id), dialog = $('speaker-sound-dialog'), trigger = $('open-speaker-sound'), container = $('speaker-sound-rooms');
    let opener = null, shown = null, reviewFocus = null, signature = '';
    const nodes = new Map(), control = model({ call, controller, now, changed: render });
    const element = (tag, className, text) => {
      const node = doc.createElement(tag); node.className = className || '';
      if (text !== undefined) node.textContent = text;
      return node;
    };
    function createRow(value) {
      const node = element('article', 'sound-room'), name = element('h3', '', value.name);
      const status = element('p', 'sound-room-status'), inputs = {}, outputs = {};
      node.append(name, status);
      for (const field of ['bass', 'treble']) {
        const label = element('label', 'sound-room-slider'), range = element('input'), output = element('output');
        range.type = 'range'; range.min = '-10'; range.max = '10'; range.step = '1'; range.disabled = true;
        range.setAttribute('aria-label', value.target + ' ' + field);
        range.addEventListener('input', event => control.choose(value.target, field, Number(event.target.value)));
        label.append(element('span', '', field === 'bass' ? 'Bass' : 'Treble'), range, output); node.append(label);
        inputs[field] = range; outputs[field] = output;
      }
      const loudnessLabel = element('label', 'sound-room-loudness'), loudness = element('input');
      loudness.type = 'checkbox'; loudness.disabled = true; loudness.setAttribute('aria-label', value.target + ' loudness');
      loudness.addEventListener('change', event => control.choose(value.target, 'loudness', event.target.checked));
      loudnessLabel.append(loudness, element('span', '', 'Loudness compensation'));
      const actions = element('div', 'sound-room-actions'), review = element('button', 'outline-button', 'Review sound');
      const flat = element('button', 'text-button', 'Flat sound'), discard = element('button', 'text-button', 'Discard edits');
      for (const button of [review, flat, discard]) button.type = 'button';
      review.addEventListener('click', () => control.review(value.target));
      flat.addEventListener('click', () => control.reset(value.target, true));
      discard.addEventListener('click', () => control.reset(value.target));
      actions.append(review, flat, discard); node.append(loudnessLabel, actions);
      return { node, status, inputs, outputs, loudness, review, flat, discard };
    }
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || controller.state.busy;
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('speaker-sound-target').textContent = s.label; $('speaker-sound-read').disabled = !control.canRead();
      const next = JSON.stringify(s.rows?.map(value => value.target) || []);
      if (next !== signature) {
        signature = next; nodes.clear(); container.replaceChildren();
        for (const value of s.rows || []) { const parts = createRow(value); nodes.set(value.target, parts); container.append(parts.node); }
        if (!s.rows) container.append(element('p', 'small-note', 'Read the selected group’s speaker sound to begin.'));
      }
      for (const value of s.rows || []) {
        const parts = nodes.get(value.target), wanted = control.desired(value.target), enabled = control.editable() && !!wanted;
        for (const field of ['bass', 'treble']) {
          parts.inputs[field].disabled = !enabled;
          if (parts.inputs[field].value !== String(wanted?.[field] ?? 0)) parts.inputs[field].value = String(wanted?.[field] ?? 0);
          parts.outputs[field].textContent = wanted ? signed(wanted[field]) + (wanted[field] !== value[field] ? ' · not applied' : '') : '—';
        }
        parts.loudness.disabled = !enabled; parts.loudness.checked = wanted?.loudness ?? false;
        parts.status.textContent = value.error || `${s.stale ? 'Last observed · ' : ''}${describe(value)}`;
        parts.review.disabled = parts.discard.disabled = !control.canReview(value.target);
        parts.flat.disabled = !enabled;
      }
      const preview = $('speaker-sound-preview'); preview.hidden = !s.pending;
      $('speaker-sound-preview-title').textContent = s.pending ? s.pending.room.name + ' · speaker sound' : '';
      $('speaker-sound-before').textContent = s.pending ? 'Current: ' + describe(s.pending.room) + '.' : '';
      $('speaker-sound-after').textContent = s.pending ? 'Requested: ' + describe(s.pending.wanted) + '.' : '';
      $('speaker-sound-confirm').disabled = !control.canApply();
      $('speaker-sound-confirm').textContent = s.writing ? 'Applying…' : 'Confirm sound change';
      $('speaker-sound-result').textContent = s.result ? `${s.result.target}: ${describe(s.result.eq)}. ${s.result.done}` : '';
      $('speaker-sound-message').textContent = s.message;
      dialog.setAttribute('aria-busy', String(s.loading || s.writing));
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('speaker-sound-read').addEventListener('click', () => { void control.read(); });
    $('speaker-sound-back').addEventListener('click', () => control.cancelReview());
    $('speaker-sound-confirm').addEventListener('click', () => { void control.apply(); });
    $('close-speaker-sound').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { MAX_AGE, observed, confirmed, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardEqualizer;
