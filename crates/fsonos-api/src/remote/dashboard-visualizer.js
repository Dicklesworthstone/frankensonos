// Sculptural ambient fields; only explicitly supplied microphone samples are audio.
'use strict';
const DashboardVisualizer = (() => {
  const MODES = ['aurora', 'orbit', 'contour'];
  function dimensions(width, height, ratio = 1) {
    const w = Math.max(1, Number.isFinite(width) ? width : 1), h = Math.max(1, Number.isFinite(height) ? height : 1);
    const scale = Math.min(2, Math.max(1, Number.isFinite(ratio) ? ratio : 1), Math.sqrt(1600000 / (w * h)));
    return [Math.max(1, Math.floor(w * scale)), Math.max(1, Math.floor(h * scale))];
  }
  function create(canvas, { raf = requestAnimationFrame, caf = cancelAnimationFrame,
    now = () => performance.now(), reduced = false } = {}) {
    let ctx;
    try { ctx = canvas.getContext('2d', { alpha: false }); } catch (_) { ctx = null; }
    let mode = 'aurora', seed = 0, playing = false, paused = reduced, visible = true, immersive = false;
    let disposed = false, frame = null, last = 0, time = 1200, audio = null, energy = 0;
    const stop = () => { if (frame !== null) caf(frame); frame = null; };
    function draw() {
      if (!ctx || disposed || !visible) return;
      const w = canvas.width, h = canvas.height, small = w / h < 1.15;
      const cx = immersive ? w * .52 : small ? w * .57 : w * .74;
      const cy = immersive ? h * .44 : small ? h * .31 : h * .46;
      const size = immersive ? Math.min(w * .36, h * .35) : small ? Math.min(w * .39, h * .26) : Math.min(w * .26, h * .33);
      let sample = null;
      try { sample = audio?.(); } catch (_) { /* A stopped device is not a render failure. */ }
      energy += (Math.min(1, Math.max(0, sample?.energy || 0)) - energy) * .2;
      const t = time * .00016, phase = (seed % 137) / 137;
      ctx.fillStyle = '#10201f'; ctx.fillRect(0, 0, w, h);
      const ambient = ctx.createRadialGradient(cx, cy, 0, cx, cy, Math.max(w, h) * .68);
      ambient.addColorStop(0, '#2c4a40'); ambient.addColorStop(.45, '#182e29'); ambient.addColorStop(1, '#101d1d');
      ctx.fillStyle = ambient; ctx.fillRect(0, 0, w, h);
      // Fine, stationary grain gives depth without flashing or pretending to measure sound.
      for (let n = 0; n < 180; n++) {
        const x = ((n * 1877 + 317) % 1009) / 1009 * w, y = ((n * 991 + 17) % 997) / 997 * h;
        ctx.fillStyle = `rgba(210,231,187,${n % 5 === 0 ? .16 : .065})`;
        ctx.fillRect(x, y, Math.max(.6, w / 1500), Math.max(.6, w / 1500));
      }
      const ax = -.6 + Math.sin(t * .43) * .08, az = -.45 + Math.cos(t * .27) * .09;
      function project(x, y, z) {
        const y1 = y * Math.cos(ax) - z * Math.sin(ax), z1 = y * Math.sin(ax) + z * Math.cos(ax);
        const x2 = x * Math.cos(az) - y1 * Math.sin(az), y2 = x * Math.sin(az) + y1 * Math.cos(az);
        const perspective = 3.8 / (3.8 + z1);
        return [cx + x2 * size * perspective, cy + y2 * size * perspective, z1];
      }
      if (mode === 'contour') {
        for (let row = 0; row < 65; row++) {
          const v = row / 64 * 2 - 1;
          ctx.beginPath();
          for (let col = 0; col <= 140; col++) {
            const u = col / 140 * 3 - 1.5;
            const z = Math.exp(-(u * u + v * v) * 1.5) * (.8 + .2 * Math.sin(u * 5 + t))
              + Math.cos(u * 5 + v * 4 + t) * .1 + energy * .2;
            const p = project(u, v * .85 - z * .45, z);
            if (col === 0) ctx.moveTo(p[0], p[1]); else ctx.lineTo(p[0], p[1]);
          }
          ctx.strokeStyle = `rgba(${155 + row},${177 + row},${141 + row},${.18 + Math.sin(row / 64 * Math.PI) * .57})`;
          ctx.lineWidth = Math.max(.6, w / 1100); ctx.stroke();
        }
      } else {
        const rows = mode === 'orbit' ? 48 : 40, cols = 128, points = [], quads = [];
        for (let j = 0; j <= rows; j++) {
          const v = j / rows * Math.PI * 2;
          const ring = [];
          for (let k = 0; k <= cols; k++) {
            const u = k / cols * Math.PI * 2;
            const bin = sample?.bins?.[Math.min(63, Math.floor(k / cols * 63))] || 0;
            const radius = .87 + Math.sin(u * 3 + t + phase) * .06;
            const tube = (mode === 'orbit' ? .18 : .31) + energy * .065 + bin * .07;
            const ripple = mode === 'orbit' ? Math.sin(v * 13) * .02 : Math.cos(v * 7 + u * 2) * .015;
            const twist = v + Math.sin(u * 2 + t) * .3;
            const p = project((radius + (tube + ripple) * Math.cos(twist)) * Math.cos(u),
              (radius + (tube + ripple) * Math.cos(twist)) * Math.sin(u),
              tube * Math.sin(twist) + Math.sin(u * 3 + t * .6) * (mode === 'orbit' ? .08 : .2));
            ring.push(p);
          }
          points.push(ring);
        }
        for (let j = 0; j < rows; j++) for (let k = 0; k < cols; k++) {
          const p = [points[j][k], points[j][k + 1], points[j + 1][k + 1], points[j + 1][k]];
          quads.push({ p, z: p.reduce((n, a) => n + a[2], 0), j, k });
        }
        quads.sort((a, b) => b.z - a.z);
        for (const { p, j, k } of quads) {
          const u = k / cols * Math.PI * 2, v = j / rows * Math.PI * 2;
          const nx = Math.cos(v) * Math.cos(u), ny = Math.cos(v) * Math.sin(u), nz = Math.sin(v);
          const ry = ny * Math.cos(ax) - nz * Math.sin(ax), rz = ny * Math.sin(ax) + nz * Math.cos(ax);
          const rx = nx * Math.cos(az) - ry * Math.sin(az);
          const lit = Math.max(0, -rx * .5 - ry * .6 - rz * .5);
          const specular = Math.pow(Math.max(0, -rz), 14) * .3;
          const shine = Math.min(1, .05 + lit * .88 + specular);
          const rib = mode === 'orbit' && j % 3 === 0 ? .68 : 1;
          const warm = Math.pow(Math.max(0, Math.cos(u + 1.5)), 8) * .45;
          const r = (32 + shine * 196 + warm * 39) * rib, g = (71 + shine * 171 - warm * 24) * rib, b = (57 + shine * 148 - warm * 39) * rib;
          ctx.fillStyle = `rgb(${Math.round(r)},${Math.round(g)},${Math.round(b)})`;
          ctx.beginPath(); ctx.moveTo(p[0][0], p[0][1]);
          for (let n = 1; n < p.length; n++) ctx.lineTo(p[n][0], p[n][1]);
          ctx.closePath(); ctx.fill();
          ctx.strokeStyle = ctx.fillStyle; ctx.lineWidth = .6; ctx.stroke();
        }
        // Sparse copper trajectories complement the physical ribbon rather than fake FFT bars.
        for (let j = 0; j < 3; j++) {
          ctx.beginPath();
          for (let k = 0; k <= 160; k++) {
            const a = k / 160 * Math.PI * 2, r = 1.42 + j * .055;
            const p = project(Math.cos(a) * r, Math.sin(a) * r * .68, Math.sin(a + t) * .15);
            if (!k) ctx.moveTo(p[0], p[1]); else ctx.lineTo(p[0], p[1]);
          }
          ctx.strokeStyle = `rgba(220,177,116,${.27 - j * .07})`; ctx.lineWidth = Math.max(.6, w / 1600); ctx.stroke();
        }
      }
      const veil = ctx.createLinearGradient(0, 0, 0, h);
      veil.addColorStop(0, '#0d18151a'); veil.addColorStop(.6, '#0d181500'); veil.addColorStop(1, '#0b171b99');
      ctx.fillStyle = veil; ctx.fillRect(0, 0, w, h);
    }
    function loop(stamp) {
      frame = null;
      if (disposed || !visible || paused || !ctx) return;
      if (stamp - last >= 1000 / 24) {
        time += Math.min(90, Math.max(0, stamp - last)) * (playing || audio ? 1 : .22);
        last = stamp; draw();
      }
      frame = raf(loop);
    }
    function start() { stop(); draw(); if (!disposed && visible && !paused && ctx) { last = now(); frame = raf(loop); } }
    return { supported: !!ctx,
      resize(width, height, ratio) { [canvas.width, canvas.height] = dimensions(width, height, ratio); draw(); },
      mode(value) { if (MODES.includes(value)) { mode = value; draw(); } },
      track(value, isPlaying) { seed = value >>> 0; playing = !!isPlaying; draw(); },
      audio(reader) { audio = typeof reader === 'function' ? reader : null; draw(); },
      pause(value) { paused = !!value; start(); },
      visible(value) { visible = !!value; if (visible) start(); else stop(); },
      immersive(value) { immersive = !!value; draw(); },
      dispose() { disposed = true; audio = null; stop(); },
    };
  }
  return { MODES, dimensions, create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardVisualizer;
