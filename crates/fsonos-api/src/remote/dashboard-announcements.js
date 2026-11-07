// Review explicit physical rooms, then let the daemon play and restore the house.
'use strict';
const DashboardAnnouncements = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const A = typeof AnnouncementRemote !== 'undefined' ? AnnouncementRemote : require('./announcements.js');
  const MAX_AGE = 45000;
  const fail = message => { throw new Error(message); };
  const signature = groups => JSON.stringify(groups.map(C.shape).sort());
  function roomsOf(value, expected) {
    const groups = C.zones(value);
    if (signature(groups) !== expected) fail('The house grouping changed. Refresh playback and read rooms again.');
    return Object.freeze(A.roomsOf(groups));
  }
  function reportOf(value, reviewed) {
    const report = A.reportOf(value);
    // Native reports keep every requested room even on playback/restore failure.
    // They omit household IDs, so only the name multiset can be cross-checked.
    const expected = reviewed.rooms.map(room => room.name).sort();
    const actual = report.households.flatMap(household => household.rooms).sort();
    if (expected.length !== actual.length || expected.some((name, i) => name !== actual[i])) {
      fail('The announcement result did not account for every reviewed room. Its outcome is uncertain.');
    }
    return report;
  }
  function model({ call, controller, changed = () => {}, now = () => performance.now(), readFile = A.readLocalFile }) {
    let version = 0, active = true, disposed = false, reading = null, fileRead = null, readAt = 0;
    const state = { open: false, target: '', scope: '', rooms: [], selected: new Set(), stale: true,
      source: 'speech', words: '', voice: '', chime: 'bell', volume: '35', wav: null, fileLoading: false,
      loading: false, writing: false, phase: '', pending: null, report: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const live = () => !disposed && active && state.open;
    const scope = () => signature(controller.state.items.map(item => item.zone));
    const matching = () => state.target === controller.state.selected && state.scope === scope();
    const canRead = () => !!(live() && controller.selected()?.fresh && controller.state.active && controller.state.topologyFresh
      && !controller.state.busy && !controller.state.loading && !state.loading && !state.writing);
    const editable = () => !!(live() && !state.loading && !state.writing && !controller.state.busy);
    const ready = () => !!(editable() && !state.fileLoading && matching() && !state.stale
      && controller.state.topologyFresh && now() - readAt < MAX_AGE && controller.canControl());
    function stopFile() {
      const work = fileRead; fileRead = null; state.fileLoading = false;
      if (work) work.cancel();
    }
    function retire(message, discard = false) {
      ++version; reading?.abort(); reading = null; state.loading = false; state.pending = null; state.stale = true;
      if (discard) { state.rooms = []; state.selected = new Set(); readAt = 0; stopFile(); state.wav = null; }
      if (message) state.message = message;
    }
    function observe() {
      if (disposed) return;
      if (!matching()) {
        retire('The selected group or house grouping changed. Read rooms and review again.', true);
        state.target = controller.state.selected; state.scope = scope();
      } else if (!state.writing && (state.loading || state.pending || !state.stale)
          && (!controller.selected()?.fresh || !controller.state.active || !controller.state.topologyFresh
            || (!state.stale && now() - readAt >= MAX_AGE))) {
        retire('Read the current rooms before announcing. Refresh playback first if it is stale.');
      }
      publish();
    }
    async function read() {
      if (!canRead()) return false;
      retire(); state.target = controller.state.selected; state.scope = scope();
      const v = version, expected = state.scope, abort = new AbortController(), started = now();
      reading = abort; state.loading = true; state.message = 'Reading physical rooms and their groups…'; publish();
      try {
        const value = await call('/zones', undefined, { signal: abort.signal });
        if (!live() || v !== version || !matching() || !controller.selected()?.fresh || !controller.state.active
            || controller.state.busy || !controller.state.topologyFresh || now() - started >= MAX_AGE) {
          fail('The room observation is no longer current. Read rooms again.');
        }
        state.rooms = roomsOf(value, expected);
        state.selected = new Set([...state.selected].filter(target => state.rooms.some(room => room.target === target)));
        state.stale = false; readAt = started; state.message = ''; return true;
      } catch (error) {
        if (live() && v === version) state.message = C.problem(error);
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); }
      }
    }
    function edit(field, value) {
      if (!editable() || typeof value !== 'string' || !['source', 'words', 'voice', 'chime', 'volume'].includes(field)
          || (field === 'source' && !['speech', 'chime', 'wav'].includes(value))) return false;
      if (field === 'source' && value !== state.source) { stopFile(); state.wav = null; }
      ++version; state.pending = null; state[field] = value; state.message = ''; publish(); return true;
    }
    function select(target, checked) {
      if (!editable() || state.stale || typeof checked !== 'boolean' || !state.rooms.some(room => room.target === target)) return false;
      if (checked) state.selected.add(target); else state.selected.delete(target);
      ++version; state.pending = null; state.message = ''; publish(); return true;
    }
    async function chooseWav(file) {
      if (!editable() || state.source !== 'wav') return false;
      stopFile(); ++version; state.pending = null; state.wav = null; state.message = '';
      let work;
      try {
        if (!file || !Number.isInteger(file.size) || file.size < 12 || file.size > A.MAX_WAV_BYTES) {
          fail('Choose a WAV file no larger than 16 MiB. Nothing has been uploaded.');
        }
        work = readFile(file); fileRead = work; state.fileLoading = true; publish();
        const buffer = await work.promise;
        if (fileRead !== work || !live() || state.source !== 'wav') return false;
        const bytes = new Uint8Array(buffer);
        if (bytes.length !== file.size) fail('The selected file changed during reading. Choose it again.');
        state.wav = Object.freeze({ ...A.wavInfo(bytes), name: typeof file.name === 'string' ? file.name : 'Selected WAV',
          encoded: A.wavBase64(bytes) });
        state.message = 'WAV checked locally. Review the rooms and clip before uploading.'; return true;
      } catch (error) {
        if (live() && (!work || fileRead === work)) state.message = C.problem(error);
        return false;
      } finally {
        if (fileRead === work) { fileRead = null; state.fileLoading = false; }
        publish();
      }
    }
    function clearWav() {
      if (!editable()) return false;
      stopFile(); ++version; state.wav = null; state.pending = null; state.message = ''; publish(); return true;
    }
    function canReview() {
      if (!ready()) return false;
      try { A.proposal(state); return true; } catch (_) { return false; }
    }
    function review() {
      if (!ready()) return null;
      state.pending = null;
      try {
        state.pending = Object.freeze({ ...A.proposal(state), version, target: state.target, scope: state.scope });
        state.message = '';
      } catch (error) { state.message = C.problem(error); }
      publish(); return state.pending;
    }
    const canApply = () => !!(ready() && state.pending && state.pending.version === version);
    async function apply(reviewed = state.pending) {
      if (!canApply() || reviewed !== state.pending) return false;
      const v = version, abort = new AbortController();
      reading = abort;
      let receipt = null;
      state.pending = null; state.writing = true; state.phase = 'checking'; state.message = 'Checking the selected rooms…'; publish();
      try {
        const ok = await controller.mutate(async ({ target, check, send }) => {
          const valid = () => {
            check();
            if (!live() || v !== version || target !== reviewed.target || !matching() || !controller.state.topologyFresh
                || now() - readAt >= MAX_AGE) fail('The announcement review is no longer current. Read rooms and review again.');
          };
          valid();
          const current = roomsOf(await call('/zones', undefined, { signal: abort.signal }), reviewed.scope); valid();
          if (reviewed.rooms.some(previous => !current.some(room => room.target === previous.target && room.signature === previous.signature))) {
            fail('A selected room or group changed. Read rooms and review again.');
          }
          state.wav = null; state.report = null; state.phase = 'announcing';
          state.message = 'The daemon is preparing, playing, and restoring. This can take several minutes. Do not send the announcement again.'; publish();
          valid(); reading = null;
          receipt = reportOf(await send('/announce', reviewed.body), reviewed);
          return { done: receipt.clean ? 'Announcement finished; the daemon reports playback restored.'
            : 'The announcement did not complete cleanly. Check playback and restoration details.', changed: true };
        }, { wholeHouse: true });
        if (!disposed) {
          if (ok) state.report = receipt;
          state.message = controller.state.message || 'No announcement was sent. Refresh playback and read rooms again.';
        }
        return ok;
      } finally {
        state.writing = false; state.phase = ''; retire(); publish();
      }
    }
    return { state, read, canRead, editable, edit, select, chooseWav, clearWav, canReview, review, canApply, apply, observe,
      cancelReview() { if (state.writing) return false; ++version; state.pending = null; publish(); return true; },
      open() {
        if (disposed || !active) return Promise.resolve(false);
        state.open = true;
        if (state.writing) { publish(); return Promise.resolve(false); }
        retire(undefined, true); state.target = controller.state.selected; state.scope = scope();
        if (!canRead()) { state.message = 'Refresh playback, then read rooms before announcing. The last received result remains below.'; publish(); return Promise.resolve(false); }
        publish(); return read();
      },
      close() { state.open = false; retire(undefined, true); publish(); },
      suspend() { active = false; state.open = false; retire(undefined, true); publish(); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(undefined, true); },
    };
  }

  function panel({ call, controller, document: doc = document, now, readFile }) {
    const $ = id => doc.getElementById(id), dialog = $('announcements-dialog'), trigger = $('open-announcements'), preview = $('announcement-preview');
    let opener = null, reviewFocus = null, shown = null, roomsShown = null, reportShown = null;
    const checks = new Map();
    const element = (tag, text) => { const node = doc.createElement(tag); if (text !== undefined) node.textContent = text; return node; };
    const control = model({ call, controller, now, readFile, changed: render });
    function render(s) {
      trigger.disabled = !controller.selected() || !controller.state.active || (controller.state.busy && !s.writing);
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) { dialog.close(); if (opener && !opener.disabled) opener.focus(); }
      $('announcement-read').disabled = !control.canRead();
      const disabled = !control.editable();
      for (const field of ['source', 'words', 'voice', 'chime', 'volume']) {
        const input = $('announcement-' + field); input.disabled = disabled;
        if (input.value !== s[field]) input.value = s[field];
      }
      $('announcement-speech').hidden = s.source !== 'speech';
      $('announcement-chimes').hidden = s.source !== 'chime';
      $('announcement-wav').hidden = s.source !== 'wav';
      $('announcement-file').disabled = disabled;
      $('announcement-clear-file').disabled = disabled || (!s.wav && !s.fileLoading);
      $('announcement-file-info').textContent = s.fileLoading ? 'Checking the selected file locally…' : s.wav
        ? `${s.wav.name} · ${s.wav.seconds.toFixed(2)} seconds · ${s.wav.channels} channel(s) · ${s.wav.rate} Hz · ${s.wav.size} bytes`
        : 'Mono or stereo 16-bit PCM WAV, up to 16 MiB and five minutes. Nothing is uploaded until you confirm.';
      if (!s.wav && !s.fileLoading) $('announcement-file').value = '';
      if (roomsShown !== s.rooms) {
        roomsShown = s.rooms; checks.clear(); $('announcement-rooms').replaceChildren();
        for (const room of s.rooms) {
          const label = element('label'), input = element('input'); input.type = 'checkbox';
          input.addEventListener('change', () => control.select(room.target, input.checked));
          label.append(input, element('span', room.label + ' · group: ' + room.group));
          $('announcement-rooms').append(label); checks.set(room.target, input);
        }
      }
      for (const [target, input] of checks) { input.checked = s.selected.has(target); input.disabled = disabled || s.stale; }
      $('announcement-context').textContent = `${s.selected.size} of ${s.rooms.length} physical rooms selected${s.stale ? ' · read rooms again before announcing' : ''}.`;
      $('announcement-review').disabled = !control.canReview();
      $('announcement-message').textContent = s.message;
      preview.hidden = !s.pending;
      if (s.pending) {
        $('announcement-preview-source').textContent = s.pending.label;
        $('announcement-preview-rooms').textContent = s.pending.rooms.map(room => room.label).join(' · ');
        $('announcement-preview-groups').textContent = [...new Set(s.pending.rooms.map(room => `${room.group} (${room.household})`))].join(' · ');
        $('announcement-preview-level').textContent = `Requested volume ${s.pending.body.volume}. The daemon applies room safety limits, then attempts to restore playback, grouping and levels.`;
      }
      $('announcement-confirm').disabled = !control.canApply(); $('announcement-back').disabled = s.writing;
      $('announcement-result').hidden = !s.report;
      if (reportShown !== s.report) {
        reportShown = s.report; $('announcement-result-detail').replaceChildren();
        $('announcement-result-title').textContent = s.report ? s.report.clean ? 'Announcement restored' : 'Check announcement results' : '';
        if (s.report) {
          $('announcement-result-detail').append(element('p', s.report.done));
          for (const h of s.report.households) {
            const section = element('section');
            section.append(element('h4', h.rooms.join(' + ') + ' · ' + ({ finished: 'Finished', deadline: 'Playback timed out', never_started: 'Playback did not start', failed: 'Playback failed' })[h.outcome]), element('p', 'Lead room: ' + h.lead),
              element('p', 'Applied levels: ' + (h.levels.map(level => `${level.room}: ${level.volume}`).join(' · ') || 'not reported')));
            if (h.error) section.append(element('p', h.error));
            if (h.not_restored.length) section.append(element('p', 'Not restored: ' + h.not_restored.join('; ')));
            $('announcement-result-detail').append(section);
          }
        }
      }
      if (s.pending && s.pending !== shown) { reviewFocus = doc.activeElement; preview.focus(); }
      else if (!s.pending && shown && s.open && reviewFocus && !reviewFocus.disabled) reviewFocus.focus();
      shown = s.pending;
    }
    trigger.addEventListener('click', () => { opener = doc.activeElement; void control.open(); });
    $('announcement-read').addEventListener('click', () => { void control.read(); });
    for (const field of ['source', 'words', 'voice', 'chime', 'volume']) {
      $('announcement-' + field).addEventListener(['source', 'chime'].includes(field) ? 'change' : 'input', event => control.edit(field, event.target.value));
    }
    $('announcement-file').addEventListener('change', event => { void control.chooseWav(event.target.files?.[0]); });
    $('announcement-clear-file').addEventListener('click', () => control.clearWav());
    $('announcement-review').addEventListener('click', () => control.review());
    $('announcement-back').addEventListener('click', () => control.cancelReview());
    $('announcement-confirm').addEventListener('click', () => { void control.apply(); });
    $('close-announcements').addEventListener('click', () => control.close());
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    render(control.state); return control;
  }
  return { MAX_AGE, signature, roomsOf, reportOf, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardAnnouncements;
