// Optional local microphone analysis. No recording, playback, storage or network transport.
'use strict';
const DashboardAudio = (() => {
  function create({ media = navigator.mediaDevices, Context = window.AudioContext || window.webkitAudioContext,
    secure = window.isSecureContext, changed = () => {}, schedule = setTimeout, cancel = clearTimeout } = {}) {
    let generation = 0, current = null, disposed = false, deadline = null;
    const state = { status: 'idle', message: 'Microphone off. Ambient visuals do not analyze Sonos audio.' };
    const emit = () => { if (!disposed) { try { changed({ ...state }); } catch (_) { /* View only. */ } } };
    function release(session) {
      if (!session) return;
      for (const track of session.stream?.getTracks() || []) track.stop();
      try { session.source?.disconnect(); } catch (_) { /* Already detached. */ }
      try { session.analyser?.disconnect(); } catch (_) { /* Already detached. */ }
      try { Promise.resolve(session.context.close()).catch(() => {}); } catch (_) { /* Already closed. */ }
    }
    function stop(message = 'Microphone off. Nothing was recorded or uploaded.') {
      ++generation;
      if (deadline !== null) cancel(deadline); deadline = null;
      const previous = current; current = null; release(previous);
      state.status = 'idle'; state.message = message; emit();
    }
    async function start() {
      if (disposed || state.status === 'requesting' || state.status === 'live') return false;
      if (!secure || !media?.getUserMedia || !Context) {
        state.status = 'error'; state.message = 'Microphone analysis needs a supported browser on localhost or HTTPS. Ambient mode is still available.'; emit(); return false;
      }
      const id = ++generation, session = { context: null, stream: null, source: null, analyser: null };
      current = session; state.status = 'requesting'; state.message = 'Allow microphone access in your browser, or cancel below.'; emit();
      const valid = () => !disposed && generation === id && current === session;
      try {
        // Resume from the user's gesture, before waiting on the permission dialog.
        session.context = new Context();
        const resumed = session.context.resume();
        Promise.resolve(resumed).catch(() => {});
        const expired = new Promise((_, reject) => {
          deadline = schedule(() => reject(new Error('Microphone permission timed out. Choose Use microphone to try again.')), 30000);
        });
        const grant = media.getUserMedia({ audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false }, video: false })
          .then((stream) => {
            // A dismissed/late permission result must never leave a device open.
            if (!valid()) { for (const track of stream.getTracks()) track.stop(); return null; }
            session.stream = stream; return stream;
          });
        const stream = await Promise.race([grant, expired]);
        if (!valid() || !stream) return false;
        await Promise.race([resumed, expired]);
        if (!valid()) return false;
        if (deadline !== null) cancel(deadline); deadline = null;
        session.source = session.context.createMediaStreamSource(stream);
        session.analyser = session.context.createAnalyser();
        session.analyser.fftSize = 2048; session.analyser.smoothingTimeConstant = .8;
        // Deliberately not connected to context.destination: no feedback loop.
        session.source.connect(session.analyser);
        session.wave = new Uint8Array(session.analyser.fftSize);
        session.frequency = new Uint8Array(session.analyser.frequencyBinCount);
        session.bins = new Float32Array(64);
        for (const track of stream.getTracks()) track.addEventListener('ended', () => {
          if (valid()) stop('The microphone disconnected. Ambient visuals are still available.');
        }, { once: true });
        state.status = 'live'; state.message = 'Live microphone · analyzed on this device only. Not a direct Sonos feed.'; emit(); return true;
      } catch (e) {
        if (!valid()) return false;
        stop(); state.status = 'error';
        state.message = e?.name === 'NotAllowedError' ? 'Microphone permission was denied. Ambient mode works without it; enable permission in your browser to try again.'
          : e?.name === 'NotFoundError' ? 'No microphone was found. Connect an input device or keep using ambient mode.'
            : e?.message || 'The microphone could not start. Ambient mode is still available.';
        emit(); return false;
      }
    }
    function sample() {
      const s = current;
      if (state.status !== 'live' || !s?.analyser) return null;
      s.analyser.getByteTimeDomainData(s.wave); s.analyser.getByteFrequencyData(s.frequency);
      let power = 0;
      for (const value of s.wave) power += ((value - 128) / 128) ** 2;
      for (let i = 0; i < s.bins.length; i++) {
        const from = Math.floor((Math.exp(i / 64 * Math.log(s.frequency.length)) - 1));
        const to = Math.max(from + 1, Math.floor(Math.exp((i + 1) / 64 * Math.log(s.frequency.length)) - 1));
        let total = 0;
        for (let j = from; j < to; j++) total += s.frequency[j] || 0;
        s.bins[i] = total / (to - from) / 255;
      }
      return { energy: Math.min(1, Math.sqrt(power / s.wave.length) * 3), bins: s.bins };
    }
    return { state, start, stop, sample,
      dispose() { stop(); disposed = true; } };
  }
  return { create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardAudio;
