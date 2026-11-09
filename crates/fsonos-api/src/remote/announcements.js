// Announcements use the daemon's local speech engine and restoration path.
'use strict';

const AnnouncementRemote = (() => {
  const routes = { announceAudio: ['POST', '/announce'], announcementStart: ['POST', '/announcements'],
    announcementJobs: ['GET', '/announcements'], announcementJob: ['GET', '/announcements/{id}'],
    announcementCancel: ['POST', '/announcements/{id}/cancel'] };
  const chimes = Object.freeze(['bell', 'beep', 'rise']);
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const problem = (error) => error && typeof error.message === 'string'
    ? `${error.message}${text(error.hint) ? ` ${error.hint}` : ''}` : 'The request failed.';
  const unicode = (value) => ![...value].some((c) => c.length === 1 && /[\uD800-\uDFFF]/.test(c));

  const MAX_WAV_BYTES = 16 * 1024 * 1024;
  const sampleRates = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

  // Mirrors the daemon's complete RIFF walk for early feedback. The daemon
  // remains authoritative and validates the uploaded bytes independently.
  function wavInfo(bytes) {
    const bad = (why) => { throw new Error(`Not a supported WAV: ${why}. Use mono/stereo 16-bit PCM, at most 16 MiB and five minutes.`); };
    if (!(bytes instanceof Uint8Array) || bytes.length > MAX_WAV_BYTES) bad('invalid size');
    const id = (at) => String.fromCharCode(...bytes.subarray(at, at + 4));
    if (bytes.length < 12 || id(0) !== 'RIFF' || id(8) !== 'WAVE') bad('missing RIFF/WAVE header');
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const u16 = (at) => view.getUint16(at, true), u32 = (at) => view.getUint32(at, true);
    if (u32(4) + 8 !== bytes.length) bad('file length mismatch');
    let at = 12, format = null, data = null;
    while (at + 8 <= bytes.length) {
      const kind = id(at), length = u32(at + 4), body = at + 8, end = body + length;
      if (end > bytes.length) bad('truncated chunk');
      if (kind === 'fmt ') {
        if (format || length < 16) bad('duplicate or short format');
        const code = u16(body);
        if (code === 65534) {
          const guid = [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113];
          if (length < 40 || u16(body + 16) < 22 || u16(body + 16) + 18 > length
              || u16(body + 18) !== 16 || guid.some((b, i) => bytes[body + 24 + i] !== b)) bad('extensible format is not 16-bit PCM');
        } else if (code !== 1) bad('not PCM');
        const channels = u16(body + 2), rate = u32(body + 4), byteRate = u32(body + 8);
        const align = u16(body + 12), bits = u16(body + 14);
        if (![1, 2].includes(channels) || bits !== 16 || !sampleRates.includes(rate)) bad('unsupported channels, bits, or sample rate');
        if (align !== channels * 2 || byteRate !== rate * align) bad('inconsistent PCM alignment or byte rate');
        format = { channels, rate, byteRate, align };
      } else if (kind === 'data') {
        if (!format || data !== null || !length || length % format.align !== 0) bad('missing format, duplicate, empty, or unaligned audio');
        data = length;
      }
      at = end + (length & 1);
      if (at > bytes.length) bad('missing chunk padding');
    }
    if (at !== bytes.length || !format || data === null) bad('incomplete WAV');
    if (data > format.byteRate * 300) bad('longer than five minutes');
    return Object.freeze({ size: bytes.length, channels: format.channels, rate: format.rate,
      seconds: data / format.byteRate });
  }

  function wavBase64(bytes) {
    // Every non-final chunk has a multiple of three bytes: only the final
    // chunk may have padding. Keep the argument list and temporary strings small.
    const chunks = [];
    for (let at = 0; at < bytes.length; at += 24576) {
      chunks.push(btoa(String.fromCharCode(...bytes.subarray(at, at + 24576))));
    }
    return chunks.join('');
  }

  function readLocalFile(file) {
    const reader = new FileReader();
    let finish;
    const promise = new Promise((resolve, reject) => {
      let settled = false;
      const timer = setTimeout(() => finish(new Error('The local file read timed out. Choose the file again.')), 15000);
      finish = (error, value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        reader.onload = reader.onerror = reader.onabort = null;
        if (error) { reader.abort(); reject(error); } else resolve(value);
      };
      reader.onload = () => finish(null, reader.result);
      reader.onerror = () => finish(new Error('The selected file could not be read.'));
      reader.onabort = () => finish(new Error('The local file read was cancelled.'));
      try { reader.readAsArrayBuffer(file); } catch (error) { finish(error); }
    });
    return { promise, cancel: () => finish(new Error('The local file read was cancelled.')) };
  }

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
          || !['finished', 'deadline', 'never_started', 'failed', 'cancelled'].includes(h.outcome)
          || (h.error != null && typeof h.error !== 'string')
          || (h.not_restored != null && (!Array.isArray(h.not_restored) || !h.not_restored.every(text)))) invalid();
      const notRestored = h.not_restored || [];
      if (value.clean && (h.outcome !== 'finished' || h.error || notRestored.length)) invalid();
      return { lead: h.lead, rooms: [...h.rooms], levels: h.levels.map((l) => ({ room: l.room, volume: l.volume })),
        outcome: h.outcome, error: h.error || '', not_restored: [...notRestored] };
    });
    return { done: value.done, clean: value.clean, households };
  }

  const jobFinished = job => ['completed', 'cancelled', 'failed'].includes(job.phase);
  const sameRooms = (left, right) => {
    const expected = [...right].sort();
    return left.length === expected.length && [...left].sort().every((room, index) => room === expected[index]);
  };
  const jobIdentity = job => JSON.stringify([job.id, job.kind, job.created_at, [...job.rooms].sort()]);

  function jobOf(value) {
    const invalid = () => { throw new Error('The daemon returned an unreadable announcement job. Refresh jobs before another action.'); };
    if (!value || !/^[0-9a-f]{32}$/.test(value.id) || !text(value.done)
        || !['speech', 'chime', 'wav'].includes(value.kind)
        || !['queued', 'preparing', 'playing', 'restoring', 'completed', 'cancelled', 'failed'].includes(value.phase)
        || !Array.isArray(value.rooms) || !value.rooms.length || value.rooms.length > 512
        || !value.rooms.every(room => text(room) && room.lastIndexOf('@') > 0 && !room.endsWith('@'))
        || new Set(value.rooms).size !== value.rooms.length
        || !Number.isSafeInteger(value.created_at)
        || (value.finished_at !== null && !Number.isSafeInteger(value.finished_at))
        || typeof value.cancel_requested !== 'boolean' || typeof value.can_cancel !== 'boolean'
        || !Array.isArray(value.progress) || value.progress.length > 512) invalid();
    const terminal = jobFinished(value);
    if (value.can_cancel !== (!terminal && !value.cancel_requested)
        || terminal !== (value.finished_at !== null)) invalid();
    const progress = value.progress.map(row => {
      if (!row || (row.household !== null && !text(row.household)) || !text(row.lead)
          || !Array.isArray(row.rooms) || !row.rooms.length || row.rooms.length > 512 || !row.rooms.every(text)
          || !['queued', 'preparing', 'playing', 'restoring', 'complete'].includes(row.phase)) invalid();
      return Object.freeze({ household: row.household, lead: row.lead, rooms: Object.freeze([...row.rooms]), phase: row.phase });
    });
    const result = value.result === null ? null : reportOf(value.result);
    const error = value.error;
    if (error !== null && (!error || !text(error.code) || !text(error.detail)
        || (error.hint != null && typeof error.hint !== 'string'))) invalid();
    if ((!terminal && (result || error)) || (terminal && !result && !error)
        || (value.phase === 'completed' && (!result?.clean || error))
        || (value.phase === 'failed' && result?.clean)) invalid();
    // The job identity retains admitted qualified names. Core delivery pins
    // player IDs and may report newer display names after a queued room rename.
    if (result && result.households.reduce((count, row) => count + row.rooms.length, 0) !== value.rooms.length) invalid();
    if (value.phase === 'cancelled' && (!value.cancel_requested || !result || error
        || !result.households.some(row => row.outcome === 'cancelled')
        || result.households.some(row => !['finished', 'cancelled'].includes(row.outcome) || row.error || row.not_restored.length))) invalid();
    return Object.freeze({ id: value.id, done: value.done, kind: value.kind, rooms: Object.freeze([...value.rooms]),
      phase: value.phase, created_at: value.created_at, finished_at: value.finished_at,
      cancel_requested: value.cancel_requested, can_cancel: value.can_cancel, progress: Object.freeze(progress), result,
      error: error === null ? null : Object.freeze({ code: error.code, detail: error.detail, hint: error.hint || '' }) });
  }

  function jobsOf(value) {
    if (!Array.isArray(value) || value.length > 64) throw new Error('The announcement history is unreadable. Refresh jobs.');
    const jobs = value.map(jobOf);
    if (new Set(jobs.map(job => job.id)).size !== jobs.length) throw new Error('The announcement history repeats a job identity. Refresh jobs.');
    return jobs;
  }

  function admittedJob(value, reviewed) {
    const job = jobOf(value), kind = reviewed.body.text !== undefined ? 'speech'
      : reviewed.body.chime !== undefined ? 'chime' : 'wav';
    if (job.kind !== kind || !sameRooms(job.rooms, reviewed.body.rooms)) {
      throw new Error('The returned job does not match the reviewed source and exact rooms. Its outcome is uncertain; refresh jobs.');
    }
    return job;
  }

  // Jobs survive their creation panel. Only visible panels observe them, with
  // a finite read budget; cancellation is a single explicit request by job ID.
  function jobModel({ call, changed = () => {}, completed = () => {}, setTimer = setTimeout,
    clearTimer = clearTimeout, maxReads = 240, intervalMs = 2000 }) {
    let disposed = false, version = 0, timer = null, reading = null, remaining = maxReads;
    const state = { visible: false, loading: false, stale: true, jobs: [], cancelling: new Set(), uncertain: new Set(), message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    function stopRead() {
      ++version;
      if (timer !== null) clearTimer(timer);
      timer = null; reading?.abort(); reading = null; state.loading = false;
    }
    function notifyFinished(before, after) {
      if (after.some(job => jobFinished(job) && before.some(old => old.id === job.id && !jobFinished(old)))) {
        try { Promise.resolve(completed()).catch(() => {}); } catch (_) { /* Read-only playback refresh. */ }
      }
    }
    function schedule() {
      if (disposed || !state.visible || state.loading || timer !== null || !state.jobs.some(job => !jobFinished(job))) return;
      if (remaining <= 0) { state.message = 'Automatic job updates paused. Refresh jobs for the latest status.'; publish(); return; }
      timer = setTimer(() => { timer = null; void refresh(false); }, intervalMs);
    }
    async function refresh(reset = true) {
      if (disposed || !state.visible || state.loading || state.cancelling.size) return false;
      if (reset) remaining = maxReads;
      if (remaining <= 0) { schedule(); return false; }
      stopRead(); --remaining;
      const at = version, abort = new AbortController(); reading = abort; state.loading = true; state.message = ''; publish();
      try {
        const jobs = jobsOf(await call('announcementJobs', { signal: abort.signal }));
        if (disposed || !state.visible || at !== version) return false;
        for (const job of jobs) {
          const previous = state.jobs.find(old => old.id === job.id);
          if (previous && jobIdentity(previous) !== jobIdentity(job)) throw new Error('An announcement identity changed unexpectedly. Refresh jobs before acting.');
          if (previous && ((previous.cancel_requested && !job.cancel_requested)
              || (jobFinished(previous) && JSON.stringify(previous) !== JSON.stringify(job)))) {
            throw new Error('A completed or cancelled announcement returned an older state. Refresh jobs before acting.');
          }
        }
        const previous = state.jobs; state.jobs = jobs; state.stale = false; state.uncertain.clear();
        notifyFinished(previous, jobs); return true;
      } catch (error) {
        if (!disposed && state.visible && at === version) { state.stale = true; state.message = `${problem(error)} Read jobs again; no command was repeated.`; }
        return false;
      } finally {
        if (reading === abort) { reading = null; state.loading = false; publish(); if (!state.stale) schedule(); }
      }
    }
    function accept(value, reviewed) {
      const job = reviewed ? admittedJob(value, reviewed) : jobOf(value);
      if (disposed) return job;
      stopRead(); remaining = maxReads;
      const previous = state.jobs.find(old => old.id === job.id);
      if (previous && jobIdentity(previous) !== jobIdentity(job)) throw new Error('The admitted announcement reused a different job identity. Refresh jobs.');
      const before = state.jobs;
      state.jobs = [job, ...state.jobs.filter(old => old.id !== job.id)].slice(0, 64);
      state.stale = false; state.message = `Accepted announcement ${job.id}. Playback and restoration continue in the daemon.`;
      notifyFinished(before, state.jobs); publish(); schedule(); return job;
    }
    const canCancel = job => !disposed && state.visible && !state.stale && job?.can_cancel
      && !state.cancelling.has(job.id) && !state.uncertain.has(job.id) && state.jobs.includes(job);
    async function cancel(job) {
      if (!canCancel(job)) return false;
      stopRead(); state.cancelling.add(job.id); state.message = 'Requesting cancellation; restoration may still take time…'; publish();
      try {
        const receipt = jobOf(await call('announcementCancel', { params: { id: job.id }, body: {} }));
        if (jobIdentity(receipt) !== jobIdentity(job) || (!receipt.cancel_requested && !jobFinished(receipt))) {
          throw new Error('The cancellation receipt did not confirm this exact job. Refresh jobs.');
        }
        if (!disposed) {
          const before = state.jobs;
          state.jobs = state.jobs.map(old => old.id === job.id ? receipt : old);
          state.uncertain.delete(job.id); state.message = receipt.done;
          notifyFinished(before, state.jobs);
        }
        return true;
      } catch (error) {
        if (!disposed) { state.uncertain.add(job.id); state.message = `${problem(error)} The cancellation request was not repeated. Refresh jobs to check its outcome.`; }
        return false;
      } finally {
        state.cancelling.delete(job.id); publish(); schedule();
      }
    }
    function setVisible(visible) {
      if (disposed || state.visible === visible) return Promise.resolve(false);
      state.visible = visible;
      if (!visible) { stopRead(); state.stale = true; publish(); return Promise.resolve(false); }
      return refresh();
    }
    return { state, refresh, accept, canCancel, cancel, setVisible,
      dispose() { stopRead(); disposed = true; state.visible = false; state.stale = true; } };
  }

  function jobPanel({ call, el, completed, document: doc = document }) {
    const heading = el('h3', { text: 'Announcement jobs' }), message = el('p', { role: 'status', 'aria-live': 'polite' });
    const list = el('div', { class: 'announcement-job-list' });
    const refresh = el('button', { type: 'button', text: 'Refresh jobs', onclick: () => { void control.refresh(); } });
    const node = el('section', { class: 'announcement-jobs' }, heading,
      el('p', { text: 'Queued and active announcements can be cancelled here. The daemon finishes safe restoration before marking a job complete. Recent receipts expire when the daemon restarts.' }),
      refresh, message, list);
    const control = jobModel({ call, completed, changed: render });
    function render(state) {
      const focus = doc.activeElement?.getAttribute?.('data-announcement-job');
      refresh.disabled = state.loading || !!state.cancelling.size || !state.visible;
      message.textContent = state.loading ? 'Reading announcement jobs…' : state.message;
      list.replaceChildren();
      for (const job of state.jobs) {
        const labels = { queued: 'Waiting', preparing: 'Preparing', playing: 'Playing', restoring: 'Restoring playback',
          completed: 'Finished and restored', cancelled: 'Cancelled', failed: 'Needs attention' };
        const card = el('section', { class: 'announcement-job' }, el('h4', { text: `${labels[job.phase]} · ${job.kind}` }),
          el('p', { text: job.rooms.join(' · ') }), el('p', { class: 'empty', text: `Job ${job.id}` }));
        if (job.cancel_requested && !jobFinished(job)) card.append(el('p', { text: 'Cancellation requested. Waiting for playback and restoration to finish.' }));
        for (const row of job.progress) card.append(el('p', { text: `${row.rooms.join(' + ')}: ${row.phase}` }));
        if (job.result) {
          card.append(el('p', { text: job.result.done }));
          for (const row of job.result.households) {
            if (row.error) card.append(el('p', { text: `${row.rooms.join(' + ')}: ${row.error}` }));
            if (row.not_restored.length) card.append(el('p', { text: `Not restored: ${row.not_restored.join('; ')}` }));
          }
        }
        if (job.error) card.append(el('p', { text: `${job.error.detail} ${job.error.hint}`.trim() }));
        if (!jobFinished(job)) {
          const cancel = el('button', { type: 'button', text: state.cancelling.has(job.id) ? 'Requesting cancellation…' : 'Cancel this announcement',
            'data-announcement-job': job.id, onclick: () => { void control.cancel(job); } });
          cancel.disabled = !control.canCancel(job); card.append(cancel);
        }
        if (state.uncertain.has(job.id)) card.append(el('p', { text: 'Cancellation outcome is unknown. Refresh jobs before another attempt.' }));
        list.append(card);
      }
      if (!state.jobs.length) list.append(el('p', { text: state.stale ? 'Read jobs to see active announcements and recent results.' : 'No retained announcement jobs.' }));
      if (focus) {
        const button = list.querySelector?.(`[data-announcement-job="${focus}"]`);
        if (button && !button.disabled) button.focus({ preventScroll: true });
      }
    }
    render(control.state); return { node, control };
  }

  function proposal(state) {
    const rooms = state.rooms.filter((r) => state.selected.has(r.target));
    if (!rooms.length || rooms.length !== state.selected.size) throw new Error('Select at least one room explicitly.');
    if (!/^[0-9]{1,3}$/.test(state.volume) || Number(state.volume) > 100) throw new Error('Choose a whole-number volume from 0 to 100.');
    const body = { rooms: Object.freeze(rooms.map((r) => r.target)), volume: Number(state.volume) };
    if (new TextEncoder().encode(JSON.stringify(body)).length > 15 * 1024) throw new Error('The selected room names exceed the request budget. Select fewer rooms.');
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
    } else if (state.source === 'wav' && state.wav) {
      body.wav_base64 = state.wav.encoded;
      label = `${state.wav.name} — ${state.wav.seconds.toFixed(2)} seconds, ${state.wav.channels} channel(s), ${state.wav.rate} Hz, ${state.wav.size} bytes`;
    } else throw new Error('Choose speech, a supported chime, or a valid WAV file.');
    return Object.freeze({ body: Object.freeze(body), rooms: Object.freeze(rooms), label });
  }

  function model({ call, changed = () => {}, completed = () => {}, readFile = readLocalFile, jobs = null }) {
    let disposed = false, epoch = 0, reading = null;
    const state = { open: false, rooms: [], selected: new Set(), source: 'speech', words: '', voice: '',
      chime: 'bell', volume: '35', loading: false, writing: false, phase: '', needsRefresh: false,
      pending: null, report: null, message: '', wav: null, fileLoading: false };
    const publish = () => { if (!disposed) changed(state); };
    const ready = () => !disposed && state.open && !state.loading && !state.writing && !state.fileLoading && !state.needsRefresh;
    const current = (version) => !disposed && state.open && epoch === version;

    function stopFileRead() {
      const previous = reading;
      reading = null;
      state.fileLoading = false;
      if (previous) previous.cancel();
    }

    async function chooseWav(file) {
      if (disposed || !state.open || state.loading || state.writing || state.needsRefresh || state.source !== 'wav') return false;
      stopFileRead();
      state.pending = null;
      state.wav = null;
      state.message = '';
      let work;
      try {
        if (!file || !Number.isInteger(file.size) || file.size < 12 || file.size > MAX_WAV_BYTES) {
          throw new Error('Choose a WAV file no larger than 16 MiB. Nothing has been uploaded.');
        }
        work = readFile(file);
        reading = work;
        state.fileLoading = true;
        publish();
        const buffer = await work.promise;
        if (reading !== work || disposed || !state.open) return false;
        const bytes = new Uint8Array(buffer);
        if (bytes.length !== file.size) throw new Error('The selected file changed during reading. Choose it again.');
        const info = wavInfo(bytes);
        state.wav = Object.freeze({ ...info, name: typeof file.name === 'string' ? file.name : 'Selected WAV',
          encoded: wavBase64(bytes) });
        state.message = 'WAV checked locally. Review the rooms and clip before uploading.';
        return true;
      } catch (error) {
        if (!disposed && (!work || reading === work)) state.message = problem(error);
        return false;
      } finally {
        if (reading === work) { reading = null; state.fileLoading = false; }
        publish();
      }
    }

    function clearWav() {
      if (disposed || state.writing) return false;
      stopFileRead();
      state.wav = null;
      state.pending = null;
      publish();
      return true;
    }

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (jobs) void jobs.setVisible(true);
      if (state.writing) { publish(); return false; }
      stopFileRead();
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
      if (name === 'source' && !['speech', 'chime', 'wav'].includes(value)) return false;
      if (name === 'source' && value !== state.source) clearWav();
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
      return proposal(state);
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
        state.wav = null;
        state.phase = 'announcing';
        state.message = jobs ? 'Starting the announcement job. Do not send it again.'
          : 'The daemon is preparing, playing, and restoring. Do not send the announcement again.';
        sent = true;
        publish();
        const value = await call(jobs ? 'announcementStart' : 'announceAudio', { body: pending.body });
        if (jobs) {
          const job = jobs.accept(value, pending);
          if (!disposed) state.message = `Announcement ${job.id} was admitted. Follow playback and restoration in Announcement jobs below.`;
          return true;
        }
        const report = reportOf(value);
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
      if (jobs) void jobs.setVisible(false);
      stopFileRead();
      state.wav = null;
      ++epoch;
      state.loading = false;
      state.pending = null;
      publish();
    }

    return { state, jobs, refresh, field, select, review, cancel, confirm, close, chooseWav, clearWav,
      dispose() { disposed = true; close(); jobs?.dispose(); } };
  }

  function panel({ call, el, completed }) {
    const summary = el('summary', { text: 'Announcements' });
    const details = el('details', {}, summary);
    const node = el('section', { class: 'announcements' }, details);
    const body = el('div', { class: 'announcement-body' });
    details.append(body);
    const jobs = jobPanel({ call, el, completed });
    const control = model({ call, completed, jobs: jobs.control, changed: render });
    const button = (label, action) => el('button', { type: 'button', text: label, onclick: action });
    const refresh = button('Refresh rooms', () => control.refresh());
    const message = el('p', { role: 'status', 'aria-live': 'polite' });
    const rooms = el('fieldset', { class: 'announcement-rooms' });
    const source = el('select', { 'aria-label': 'Announcement source',
      onchange: () => control.field('source', source.value) },
      el('option', { value: 'speech', text: 'Speak text' }), el('option', { value: 'chime', text: 'Play a chime' }),
      el('option', { value: 'wav', text: 'Upload a WAV clip' }));
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
    const file = el('input', { type: 'file', accept: '.wav,audio/wav,audio/x-wav', 'aria-label': 'WAV clip',
      onchange: () => { if (file.files[0]) void control.chooseWav(file.files[0]); else control.clearWav(); } });
    const fileInfo = el('p', { role: 'status' });
    const clearFile = button('Remove selected WAV', () => control.clearWav());
    const wavBox = el('div', {}, el('label', {}, 'WAV clip (16-bit PCM)', file), fileInfo, clearFile,
      el('p', { text: 'Mono or stereo, supported 8–48 kHz rates, at most 16 MiB and five minutes. Choosing a file only reads it locally; Announce now uploads it to this daemon.' }));
    const volume = el('input', { type: 'number', min: 0, max: 100, step: 1, 'aria-label': 'Announcement volume',
      oninput: () => control.field('volume', volume.value) });
    const review = button('Review announcement', () => control.review());
    const consent = el('div');
    const report = el('div', { class: 'announcement-report' });
    body.append(el('p', { text: 'Choose the rooms explicitly. Speech is generated by the daemon’s local engine, not this browser.' }),
      refresh, message, rooms, el('label', {}, 'Source', source), speech, chimeBox, wavBox,
      el('label', {}, 'Requested volume (house caps still apply)', volume), review, consent, report, jobs.node);
    let shownRooms = null, shownPending = null, shownReport = null;
    const selectors = new Map();

    function render(state) {
      const busy = state.loading || state.writing || state.fileLoading;
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
      wavBox.hidden = state.source !== 'wav';
      file.disabled = disabled;
      clearFile.disabled = state.writing || (!state.wav && !state.fileLoading);
      if (!state.wav && !state.fileLoading) file.value = '';
      fileInfo.textContent = state.fileLoading ? 'Reading and checking the selected WAV…' : state.wav
        ? `${state.wav.name}: ${state.wav.seconds.toFixed(2)} seconds, ${state.wav.channels} channel(s), ${state.wav.rate} Hz. Not uploaded yet.`
        : 'No validated WAV selected.';
      count.textContent = `${[...state.words.trim()].length} / 1,000 characters`;
      review.disabled = disabled || !state.selected.size;
      if (shownPending !== state.pending) {
        const hadReview = shownPending !== null;
        const inside = consent.contains(document.activeElement);
        shownPending = state.pending;
        consent.replaceChildren();
        if (state.pending) {
          const pending = state.pending;
          const cancel = button('Cancel review', () => control.cancel());
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
    const visibility = () => { void jobs.control.setVisible(details.open && !document.hidden); };
    document.addEventListener('visibilitychange', visibility);
    render(control.state);
    return { node, dispose() { document.removeEventListener('visibilitychange', visibility); control.dispose(); } };
  }

  return { routes, model, panel, proposal, roomsOf, reportOf, jobOf, jobsOf, admittedJob, jobFinished, jobModel, jobPanel,
    chimes, wavInfo, wavBase64, readLocalFile, MAX_WAV_BYTES };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = AnnouncementRemote;
else if (typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, AnnouncementRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(AnnouncementRemote.panel({ call, el,
    completed: () => loadZones().catch(() => {}) }).node);
}
