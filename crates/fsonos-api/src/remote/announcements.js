// Announcements use the daemon's local speech engine and restoration path.
'use strict';

const AnnouncementRemote = (() => {
  const routes = { announceAudio: ['POST', '/announce'] };
  const chimes = Object.freeze(['bell', 'beep', 'rise']);
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const problem = (error) => error && typeof error.message === 'string'
    ? `${error.message}${text(error.hint) ? ` ${error.hint}` : ''}` : 'The request failed.';
  const unicode = (value) => ![...value].some((c) => c.length === 1 && /[\uD800-\uDFFF]/.test(c));

  function roomsOf(value) {
    if (!Array.isArray(value) || value.length > 128) throw new Error('The group list is unreadable. Refresh rooms.');
    const seen = new Set(), rooms = [];
    for (const group of value) {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length
          || group.members.length > 128 || !group.members.every(text)
          || !group.members.includes(group.coordinator_room)) {
        throw new Error('The group list is incomplete. Refresh rooms.');
      }
      const signature = JSON.stringify([group.household, group.coordinator_room, [...group.members].sort()]);
      for (const name of group.members) {
        const target = `${name}@${group.household}`;
        if (seen.has(target) || target.length > 512 || rooms.length >= 128) {
          throw new Error('The room list is ambiguous or too large. Refresh rooms.');
        }
        seen.add(target);
        rooms.push(Object.freeze({ name, target, signature, household: group.household,
          label: `${name} (${group.household})`, group: group.members.join(' + ') }));
      }
    }
    return rooms;
  }

  function reportOf(value) {
    const invalid = () => { throw new Error('The announcement returned an unreadable result. Its outcome is uncertain.'); };
    if (!value || !text(value.done) || typeof value.clean !== 'boolean'
        || !Array.isArray(value.households) || !value.households.length || value.households.length > 128) invalid();
    const households = value.households.map((h) => {
      if (!h || !text(h.lead) || !Array.isArray(h.rooms) || !h.rooms.length || !h.rooms.every(text)
          || !Array.isArray(h.levels) || h.levels.some((l) => !l || !text(l.room)
            || !Number.isInteger(l.volume) || l.volume < 0 || l.volume > 100)
          || !['finished', 'deadline', 'never_started', 'failed'].includes(h.outcome)
          || (h.error != null && typeof h.error !== 'string')
          || (h.not_restored != null && (!Array.isArray(h.not_restored) || !h.not_restored.every(text)))) invalid();
      const notRestored = h.not_restored || [];
      if (value.clean && (h.outcome !== 'finished' || h.error || notRestored.length)) invalid();
      return { lead: h.lead, rooms: [...h.rooms], levels: h.levels.map((l) => ({ room: l.room, volume: l.volume })),
        outcome: h.outcome, error: h.error || '', not_restored: [...notRestored] };
    });
    return { done: value.done, clean: value.clean, households };
  }

  function model({ call, changed = () => {}, completed = () => {} }) {
    let disposed = false, epoch = 0;
    const state = { open: false, rooms: [], selected: new Set(), source: 'speech', words: '', voice: '',
      chime: 'bell', volume: '35', loading: false, writing: false, phase: '', needsRefresh: false,
      pending: null, report: null, message: '' };
    const publish = () => { if (!disposed) changed(state); };
    const ready = () => !disposed && state.open && !state.loading && !state.writing && !state.needsRefresh;
    const current = (version) => !disposed && state.open && epoch === version;

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      const version = ++epoch;
      state.loading = true;
      state.pending = null;
      state.needsRefresh = true;
      state.message = '';
      publish();
      try {
        const rooms = roomsOf(await call('zones'));
        if (!current(version)) return false;
        state.rooms = rooms;
        state.selected = new Set([...state.selected].filter((target) => rooms.some((r) => r.target === target)));
        state.needsRefresh = false;
        return true;
      } catch (error) {
        if (current(version)) state.message = problem(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function field(name, value) {
      if (!ready() || typeof value !== 'string' || !['source', 'words', 'voice', 'chime', 'volume'].includes(name)) return false;
      if (name === 'source' && !['speech', 'chime'].includes(value)) return false;
      state[name] = value;
      state.pending = null;
      state.message = '';
      publish();
      return true;
    }

    function select(target, checked) {
      if (!ready() || typeof checked !== 'boolean' || !state.rooms.some((r) => r.target === target)) return false;
      if (checked) state.selected.add(target); else state.selected.delete(target);
      state.pending = null;
      state.message = '';
      publish();
      return true;
    }

    function selection() {
      if (!ready()) throw new Error('Refresh rooms before reviewing another announcement.');
      const rooms = state.rooms.filter((r) => state.selected.has(r.target));
      if (!rooms.length || rooms.length !== state.selected.size) throw new Error('Select at least one room explicitly.');
      if (!/^[0-9]{1,3}$/.test(state.volume) || Number(state.volume) > 100) throw new Error('Choose a whole-number volume from 0 to 100.');
      const body = { rooms: Object.freeze(rooms.map((r) => r.target)), volume: Number(state.volume) };
      let label;
      if (state.source === 'speech') {
        const words = state.words.trim(), voice = state.voice.trim();
        if (!words || [...words].length > 1000 || !unicode(words)) throw new Error('Enter between 1 and 1,000 valid text characters.');
        if (voice.length > 256 || /[\u0000-\u001F\u007F]/.test(voice) || !unicode(voice)) throw new Error('The voice name is invalid or too long.');
        body.text = words;
        if (voice) body.voice = voice;
        label = words;
      } else if (state.source === 'chime' && chimes.includes(state.chime)) {
        body.chime = state.chime;
        label = `${state.chime} chime`;
      } else throw new Error('Choose speech or a supported chime.');
      return Object.freeze({ body: Object.freeze(body), rooms: Object.freeze(rooms), label });
    }

    function review() {
      if (!ready()) return false;
      state.pending = null;
      try {
        state.pending = selection();
        state.message = '';
        state.report = null;
        publish();
        return true;
      } catch (error) { state.message = problem(error); publish(); return false; }
    }

    function cancel() {
      if (state.writing) return false;
      state.pending = null;
      publish();
      return true;
    }

    async function confirm(pending = state.pending) {
      if (!ready() || !pending || pending !== state.pending) return false;
      const version = ++epoch;
      state.writing = true;
      state.phase = 'checking';
      state.message = 'Checking the selected rooms…';
      publish();
      let sent = false;
      try {
        const rooms = roomsOf(await call('zones'));
        if (!current(version) || pending !== state.pending) return false;
        if (pending.rooms.some((previous) => !rooms.some((r) => r.target === previous.target && r.signature === previous.signature))) {
          throw new Error('A selected room or its group changed. Refresh rooms and review again; no announcement was sent.');
        }
        state.pending = null;
        state.phase = 'announcing';
        state.message = 'The daemon is preparing, playing, and restoring. Do not send the announcement again.';
        sent = true;
        publish();
        const report = reportOf(await call('announceAudio', { body: pending.body }));
        if (!disposed) {
          state.report = report;
          state.message = report.clean ? 'Announcement finished; the daemon reports playback restored.'
            : 'The announcement did not complete cleanly. Check the playback and restoration details below.';
        }
        return true;
      } catch (error) {
        if (!disposed) state.message = `${problem(error)}${sent
          ? ' The request was not repeated. Playback or restoration may still be running; check the rooms before sending again.' : ''}`;
        return false;
      } finally {
        state.writing = false;
        state.phase = '';
        state.pending = null;
        state.needsRefresh = true;
        publish();
        if (sent && !disposed) {
          try { Promise.resolve(completed()).catch(() => {}); } catch (_) { /* read-only refresh */ }
        }
      }
    }

    function close() {
      state.open = false;
      ++epoch;
      state.loading = false;
      state.pending = null;
      publish();
    }

    return { state, refresh, field, select, review, cancel, confirm, close,
      dispose() { disposed = true; close(); } };
  }

  function panel({ call, el, completed }) {
    const summary = el('summary', { text: 'Announcements' });
    const details = el('details', {}, summary);
    const node = el('section', { class: 'announcements' }, details);
    const body = el('div', { class: 'announcement-body' });
    details.append(body);
    const control = model({ call, completed, changed: render });
    const button = (label, action) => el('button', { type: 'button', text: label, onclick: action });
    const refresh = button('Refresh rooms', () => control.refresh());
    const message = el('p', { role: 'status', 'aria-live': 'polite' });
    const rooms = el('fieldset', { class: 'announcement-rooms' });
    const source = el('select', { 'aria-label': 'Announcement source',
      onchange: () => control.field('source', source.value) },
      el('option', { value: 'speech', text: 'Speak text' }), el('option', { value: 'chime', text: 'Play a chime' }));
    const words = el('textarea', { rows: 3, maxlength: 2000, 'aria-label': 'Words to speak',
      placeholder: 'Dinner is ready', oninput: () => control.field('words', words.value) });
    const voice = el('input', { type: 'text', maxlength: 256, 'aria-label': 'Speech voice',
      placeholder: 'Configured default', oninput: () => control.field('voice', voice.value) });
    const count = el('p', { class: 'empty' });
    const speech = el('div', {}, el('label', {}, 'Words to speak', words), count,
      el('label', {}, 'Voice (optional, on the daemon host)', voice));
    const chime = el('select', { 'aria-label': 'Chime', onchange: () => control.field('chime', chime.value) },
      ...chimes.map((name) => el('option', { value: name, text: name })));
    const chimeBox = el('label', {}, 'Chime', chime);
    const volume = el('input', { type: 'number', min: 0, max: 100, step: 1, 'aria-label': 'Announcement volume',
      oninput: () => control.field('volume', volume.value) });
    const review = button('Review announcement', () => control.review());
    const consent = el('div');
    const report = el('div', { class: 'announcement-report' });
    body.append(el('p', { text: 'Choose the rooms explicitly. Speech is generated by the daemon’s local engine, not this browser.' }),
      refresh, message, rooms, el('label', {}, 'Source', source), speech, chimeBox,
      el('label', {}, 'Requested volume (house caps still apply)', volume), review, consent, report);
    let shownRooms = null, shownPending = null, shownReport = null;
    const selectors = new Map();

    function render(state) {
      const busy = state.loading || state.writing;
      body.setAttribute('aria-busy', String(busy));
      refresh.disabled = busy;
      message.textContent = state.loading ? 'Reading rooms…' : state.message;
      const disabled = busy || state.needsRefresh || !state.open;
      if (shownRooms !== state.rooms) {
        shownRooms = state.rooms;
        selectors.clear();
        rooms.replaceChildren(el('legend', { text: 'Rooms to hear the announcement' }));
        for (const room of state.rooms) {
          const input = el('input', { type: 'checkbox', 'aria-label': room.label,
            onchange: () => control.select(room.target, input.checked) });
          selectors.set(room.target, input);
          rooms.append(el('label', {}, input, room.label));
        }
        if (!state.rooms.length) rooms.append(el('p', { text: 'No rooms available. Refresh after discovery.' }));
      }
      for (const [target, input] of selectors) { input.disabled = disabled; input.checked = state.selected.has(target); }
      for (const [input, key] of [[source, 'source'], [words, 'words'], [voice, 'voice'], [chime, 'chime'], [volume, 'volume']]) {
        input.disabled = disabled;
        if (input.value !== state[key]) input.value = state[key];
      }
      speech.hidden = state.source !== 'speech';
      chimeBox.hidden = state.source !== 'chime';
      count.textContent = `${[...state.words.trim()].length} / 1,000 characters`;
      review.disabled = disabled || !state.selected.size;
      if (shownPending !== state.pending) {
        const hadReview = shownPending !== null;
        const inside = consent.contains(document.activeElement);
        shownPending = state.pending;
        consent.replaceChildren();
        if (state.pending) {
          const pending = state.pending;
          const cancel = button('Cancel announcement', () => control.cancel());
          consent.append(el('div', { class: 'announcement-review', role: 'group', 'aria-label': 'Confirm announcement' },
            el('p', { text: pending.label }),
            el('p', { text: `To: ${pending.rooms.map((r) => r.label).join(', ')}. Requested volume: ${pending.body.volume}.` }),
            el('p', { text: `Affected groups: ${[...new Set(pending.rooms.map((r) => `${r.group} (${r.household})`))].join('; ')}.` }),
            el('p', { text: 'This interrupts music and may temporarily regroup rooms. The daemon attempts to restore playback afterwards. Closing this panel after sending does not cancel playback.' }),
            el('div', { class: 'row' }, cancel, button('Announce now', () => control.confirm(pending)))));
          cancel.focus({ preventScroll: true });
        } else if (hadReview && inside && !review.disabled) review.focus({ preventScroll: true });
      }
      for (const b of consent.querySelectorAll('button')) b.disabled = state.writing;
      if (shownReport !== state.report) {
        shownReport = state.report;
        report.replaceChildren();
        if (state.report) {
          report.append(el('h3', { text: state.report.clean ? 'Finished and restored' : 'Playback needs attention' }),
            el('p', { text: state.report.done }));
          for (const h of state.report.households) {
            const section = el('div', {}, el('p', { text: `${h.rooms.join(', ')} — ${h.outcome}` }),
              el('p', { text: `Applied levels: ${h.levels.map((l) => `${l.room} ${l.volume}`).join(', ') || 'not reported'}` }));
            if (h.error) section.append(el('p', { text: h.error }));
            if (h.not_restored.length) section.append(el('p', { text: `Not restored: ${h.not_restored.join('; ')}` }));
            report.append(section);
          }
        }
      }
    }
    details.addEventListener('toggle', () => { if (details.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = AnnouncementRemote;
else {
  Object.assign(ROUTES, AnnouncementRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(AnnouncementRemote.panel({ call, el,
    completed: () => loadZones().catch(() => {}) }).node);
}
