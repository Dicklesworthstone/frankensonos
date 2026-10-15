// Procedural ambient art. Measured audio, when supplied, must be labelled separately.
'use strict';
const DashboardVisualizer = (() => {
  const MODES = ['aurora', 'orbit', 'contour'];
  function dimensions(width, height, ratio = 1) {
    const scale = Math.min(2, Math.max(1, ratio), Math.sqrt(1800000 / Math.max(1, width * height)));
    return [Math.max(1, Math.round(width * scale)), Math.max(1, Math.round(height * scale))];
  }
  function create(canvas, { raf = requestAnimationFrame, caf = cancelAnimationFrame,
    now = () => performance.now(), reduced = false } = {}) {
    const ctx = canvas.getContext('2d', { alpha: false });
    let mode = 'aurora', seed = 0, playing = false, paused = reduced, visible = true;
    let disposed = false, frame = null, last = -Infinity, tick = 0, audio = null;
    const stop = () => { if (frame !== null) caf(frame); frame = null; };
    function draw(time) {
      if (!ctx || disposed) return;
      const w = canvas.width, h = canvas.height;
      const t = time * .00012, phase = (seed % 1000) / 137;
      const sample = audio?.(), energy = Math.min(1, Math.max(0, sample?.energy || 0));
      const motion = playing || sample ? 1 : .18;
      ctx.fillStyle = '#1d1812'; ctx.fillRect(0, 0, w, h);
      const glow = ctx.createRadialGradient(w * .52, h * .38, 0, w * .52, h * .38, w * .78);
      glow.addColorStop(0, '#8a5c32'); glow.addColorStop(.35, '#604126'); glow.addColorStop(1, '#171410');
      ctx.fillStyle = glow; ctx.fillRect(0, 0, w, h);
      ctx.save(); ctx.lineWidth = Math.max(.5, w / 1100);
      if (mode === 'aurora') {
        for (let j = 0; j < 56; j++) {
          const f = j / 55;
          ctx.beginPath();
          for (let k = 0; k <= 100; k++) {
            const x = k / 100;
            const y = .25 + f * .23 + Math.sin(x * 5.8 + t * motion + phase) * .13
              + Math.sin(x * 9.5 - t * .6 * motion + f * 2) * .075
              + Math.sin(f * 4 + x * 6 + t) * energy * .03;
            if (k === 0) ctx.moveTo(x * w, y * h); else ctx.lineTo(x * w, y * h);
          }
          ctx.strokeStyle = `rgba(250,${Math.round(163 + f * 60)},${Math.round(88 + f * 65)},${.07 + .38 * Math.sin(f * Math.PI)})`;
          ctx.stroke();
        }
        // A second, softer current keeps the composition alive at its edges.
        for (let j = 0; j < 26; j++) {
          ctx.beginPath();
          for (let k = 0; k <= 80; k++) {
            const x = k / 80, y = .52 + j * .006 + Math.sin(x * 6 - t * motion + phase) * .16;
            if (k === 0) ctx.moveTo(x * w, y * h); else ctx.lineTo(x * w, y * h);
          }
          ctx.strokeStyle = 'rgba(185,201,152,.075)'; ctx.stroke();
        }
      } else if (mode === 'orbit') {
        const radius = Math.min(w, h) * .19;
        ctx.translate(w * .5, h * .39);
        for (let j = 0; j < 62; j++) {
          const r = radius + j * Math.min(w, h) * .0045;
          ctx.beginPath();
          for (let k = 0; k <= 160; k++) {
            const a = k / 160 * Math.PI * 2;
            const band = sample?.bins?.[Math.floor(k / 161 * sample.bins.length)] || 0;
            const bend = 1 + Math.sin(a * 3 + t * motion + phase + j * .035) * .055 + band * .12;
            const x = Math.cos(a) * r * bend, y = Math.sin(a) * r * .68 * bend;
            if (k === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
          }
          ctx.strokeStyle = `rgba(239,${165 + j},${105 + j},${.3 * (1 - j / 80)})`; ctx.stroke();
        }
      } else {
        for (let j = 0; j < 62; j++) {
          const f = j / 62;
          ctx.beginPath();
          for (let k = 0; k <= 120; k++) {
            const x = k / 120;
            const hill = Math.exp(-Math.pow((x - .5) * 3.3, 2));
            const wave = Math.sin(x * 11 + f * 9 + t * motion + phase) * .055;
            const y = .15 + f * .72 - hill * (.15 + wave + energy * .065);
            if (k === 0) ctx.moveTo(x * w, y * h); else ctx.lineTo(x * w, y * h);
          }
          ctx.strokeStyle = `rgba(${195 + Math.round(f * 52)},${171 + Math.round(f * 24)},137,${.11 + .25 * Math.sin(f * Math.PI)})`; ctx.stroke();
        }
      }
      ctx.restore();
      const shade = ctx.createLinearGradient(0, 0, 0, h);
      shade.addColorStop(0, '#0c0b0930'); shade.addColorStop(.55, '#0c0b0900'); shade.addColorStop(1, '#0c0b09a0');
      ctx.fillStyle = shade; ctx.fillRect(0, 0, w, h);
    }
    function loop(time) {
      frame = null;
      if (disposed || !visible || paused || !ctx) return;
      if (time - last >= 1000 / 24) { tick += Math.min(80, Math.max(0, time - last)) || 0; draw(tick); last = time; }
      frame = raf(loop);
    }
    function start() {
      stop(); draw(tick);
      if (!disposed && visible && !paused && ctx) { last = now(); frame = raf(loop); }
    }
    return {
      resize(width, height, ratio) { [canvas.width, canvas.height] = dimensions(width, height, ratio); draw(tick); },
      mode(value) { if (MODES.includes(value)) { mode = value; draw(tick); } },
      track(value, isPlaying) { seed = value >>> 0; playing = !!isPlaying; draw(tick); },
      audio(reader) { audio = typeof reader === 'function' ? reader : null; draw(tick); },
      pause(value) { paused = !!value; start(); },
      visible(value) { visible = !!value; if (visible) start(); else stop(); },
      dispose() { disposed = true; audio = null; stop(); },
      supported: !!ctx,
    };
  }
  return { MODES, dimensions, create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardVisualizer;
