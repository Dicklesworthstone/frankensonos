// Saved scenes: inspect, capture, apply with fade, and remove through reviewed writes.
'use strict';

const ScenesRemote = (() => {
  const routes = {
    scene: ['GET', '/scenes/{name}'],
    sceneSave: ['PUT', '/scenes/{name}'],
    sceneApplyReviewed: ['POST', '/scenes/{name}/apply'],
    sceneDelete: ['DELETE', '/scenes/{name}'],
  };
  const text = (v, max = 512) => typeof v === 'string' && v.trim() !== '' && [...v].length <= max;
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const plain = (v) => v && typeof v === 'object' && !Array.isArray(v)
    && (Object.getPrototypeOf(v) === Object.prototype || Object.getPrototypeOf(v) === null);
  const errorText = (e) => text(e?.message, 2048)
    ? `${e.message}${text(e.hint, 2048) ? ` ${e.hint}` : ''}`
    : 'The request failed.';

  function sourceOf(v) {
    if (!plain(v) || !['keep', 'favorite', 'uri', 'dj'].includes(v.kind)) {
      throw new Error('A scene source is unreadable.');
    }
    for (const key of ['name', 'uri', 'mood']) if (v[key] != null && !text(v[key], 2048)) {
      throw new Error('A scene source field is unreadable.');
    }
    if (v.kind === 'favorite' && !v.name) throw new Error('A favorite source has no name.');
    if (v.kind === 'uri' && !v.uri) throw new Error('A URI source has no URI.');
    return Object.freeze({ kind: v.kind, name: v.name ?? null, uri: v.uri ?? null, mood: v.mood ?? null });
  }

  function settingsOf(v, kind) {
    if (!plain(v) || Object.keys(v).length > 512) throw new Error(`Scene ${kind} settings are unreadable.`);
    const out = Object.create(null);
    for (const [room, value] of Object.entries(v)) {
      if (!text(room, 512)) throw new Error(`A scene ${kind} room is unreadable.`);
      if (kind === 'volume' ? !integer(value, 0, 100) : typeof value !== 'boolean') {
        throw new Error(`A scene ${kind} value is unreadable.`);
      }
      out[room] = value;
    }
    return Object.freeze(out);
  }

  function sceneOf(v) {
    if (!plain(v) || !text(v.name, 512) || !Number.isSafeInteger(v.saved)
        || !Array.isArray(v.groups) || v.groups.length > 512) {
      throw new Error('The scene response is unreadable.');
    }
    const seen = new Set();
    const groups = v.groups.map((g) => {
      if (!plain(g) || !text(g.coordinator, 512) || !Array.isArray(g.members)
          || g.members.length > 512 || !g.members.every((m) => text(m, 512))
          || (g.playing != null && typeof g.playing !== 'boolean')) {
        throw new Error('A scene group is unreadable.');
      }
      const members = [g.coordinator, ...g.members];
      if (new Set(members).size !== members.length) throw new Error('A scene group repeats a room.');
      for (const room of members) {
        if (seen.has(room)) throw new Error('A scene assigns a room to more than one group.');
        seen.add(room);
      }
      return Object.freeze({
        coordinator: g.coordinator,
        members: Object.freeze([...g.members]),
        source: sourceOf(g.source),
        playing: g.playing ?? null,
      });
    });
    return Object.freeze({
      name: v.name,
      saved: v.saved,
      groups: Object.freeze(groups),
      volumes: settingsOf(v.volumes, 'volume'),
      mutes: settingsOf(v.mutes, 'mute'),
    });
  }

  function scenesOf(v) {
    if (!Array.isArray(v) || v.length > 10000) throw new Error('The scene list is unreadable.');
    const names = new Set();
    return Object.freeze(v.map((row) => {
      const scene = sceneOf(row);
      if (names.has(scene.name)) throw new Error('The scene list contains duplicate names.');
      names.add(scene.name);
      return scene;
    }));
  }

  function signature(scene) {
    return JSON.stringify([
      scene.name,
      scene.groups.map((g) => [g.coordinator, g.members, g.source.kind, g.source.name,
        g.source.uri, g.source.mood, g.playing]),
      Object.entries(scene.volumes),
      Object.entries(scene.mutes),
    ]);
  }

  function detail(scene) {
    const lines = [];
    for (const g of scene.groups) {
      const rooms = [g.coordinator, ...g.members].join(' + ');
      const src = g.source.kind === 'favorite' ? `favorite “${g.source.name}”`
        : g.source.kind === 'uri' ? g.source.uri
          : g.source.kind === 'dj' ? `DJ${g.source.mood ? ` (${g.source.mood})` : ''}`
            : 'keep current source';
      lines.push(`${rooms}: ${src}${g.playing == null ? '' : g.playing ? '; playing' : '; paused'}`);
    }
    const volumes = Object.entries(scene.volumes).map(([room, level]) => `${room} ${level}`).join(', ');
    const mutes = Object.entries(scene.mutes).map(([room, mute]) => `${room} ${mute ? 'muted' : 'unmuted'}`).join(', ');
    if (volumes) lines.push(`Volumes: ${volumes}`);
    if (mutes) lines.push(`Mutes: ${mutes}`);
    return lines.join('\n') || 'This scene keeps all unspecified house state.';
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, rows: [], loaded: false, loading: false, writing: false,
      pending: null, name: '', fade: '0', message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) {} } };
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const rows = scenesOf(await call('scenes'));
        if (!current(version)) return false;
        state.rows = rows;
        state.loaded = true;
        return true;
      } catch (e) {
        if (current(version)) { state.rows = []; state.message = errorText(e); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function editName(value) {
      if (disposed || state.writing) return;
      ++epoch; state.pending = null; state.name = String(value ?? ''); publish();
    }
    function editFade(value) {
      if (disposed || state.writing) return;
      ++epoch; state.pending = null; state.fade = String(value ?? ''); publish();
    }

    function review(action, name = null) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || state.pending) return false;
      if (!['capture', 'apply', 'delete'].includes(action)) return false;
      const asked = action === 'capture' ? state.name.trim() : name;
      if (!text(asked, 512)) return false;
      const row = state.rows.find((r) => r.name === asked) || null;
      if (action !== 'capture' && !row) return false;
      let fadeSecs = null;
      if (action === 'apply') {
        const value = Number(state.fade);
        if (!integer(value, 0, 60)) return false;
        fadeSecs = value;
      }
      state.pending = Object.freeze({ action, name: asked, row, fadeSecs });
      publish();
      return true;
    }

    function cancel() {
      if (disposed || state.writing) return;
      ++epoch; state.pending = null; publish();
    }

    async function confirm(reviewed) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing
          || !reviewed || reviewed !== state.pending) return false;
      const version = ++epoch;
      state.pending = null;
      state.writing = true;
      state.message = '';
      publish();
      let sent = false;
      try {
        const latest = scenesOf(await call('scenes'));
        if (!current(version)) return false;
        const now = latest.find((r) => r.name === reviewed.name) || null;
        if (reviewed.action === 'capture') {
          if ((reviewed.row == null) !== (now == null)
              || (reviewed.row && signature(reviewed.row) !== signature(now))) {
            throw new Error('That scene changed while you reviewed the capture. Refresh and review again.');
          }
          sent = true;
          const saved = sceneOf(await call('sceneSave', { params: { name: reviewed.name }, body: {} }));
          if (saved.name !== reviewed.name) throw new Error('The reply did not confirm the reviewed scene name.');
          if (current(version)) state.message = `Captured the current house as “${saved.name}”. Refresh before another scene change.`;
          return true;
        }
        if (!now || signature(now) !== signature(reviewed.row)) {
          throw new Error('That scene changed or was removed. Refresh and review it again.');
        }
        const exact = sceneOf(await call('scene', { params: { name: reviewed.name } }));
        if (signature(exact) !== signature(reviewed.row)) {
          throw new Error('That scene changed while it was being checked. Nothing was sent.');
        }
        sent = true;
        if (reviewed.action === 'apply') {
          const result = await call('sceneApplyReviewed', {
            params: { name: reviewed.name },
            body: reviewed.fadeSecs ? { fade_secs: reviewed.fadeSecs } : {},
          });
          if (!plain(result) || result.scene !== reviewed.name || typeof result.complete !== 'boolean'
              || typeof result.changed !== 'boolean' || !Array.isArray(result.steps)
              || !Array.isArray(result.failed) || !Array.isArray(result.skipped)) {
            throw new Error('The apply reply is unreadable; the outcome may be uncertain.');
          }
          const failures = result.failed.length;
          if (current(version)) state.message = failures
            ? `Applied “${reviewed.name}” in part: ${failures} step(s) failed. Read the house before retrying anything.`
            : result.done;
          return failures === 0;
        }
        const result = await call('sceneDelete', { params: { name: reviewed.name }, body: {} });
        if (!plain(result) || result.deleted !== reviewed.name) {
          throw new Error('The delete reply did not confirm the reviewed scene.');
        }
        if (current(version)) state.message = `Deleted scene “${reviewed.name}”. Refresh before another scene change.`;
        return true;
      } catch (e) {
        if (current(version)) state.message = `${errorText(e)}${sent ? ' The request was not repeated.' : ''} Refresh before another change.`;
        return false;
      } finally {
        state.writing = false;
        state.loaded = false;
        publish();
      }
    }

    return { state, refresh, editName, editFade, review, cancel, confirm,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  function panel({ call, el }) {
    const body = el('div', { class: 'scene-manager-body' });
    const node = el('details', { class: 'scene-manager scenes' },
      el('summary', { text: 'Manage scenes' }), body);
    const control = model({ call, changed: render });
    const button = (label, key, work, disabled = false) => el('button', {
      type: 'button', text: label, 'data-scene-focus': key, disabled, onclick: work,
    });
    function render(s) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-scene-focus') : null;
      body.replaceChildren();
      body.setAttribute('aria-busy', String(s.loading || s.writing));
      body.append(el('p', { text: 'Scenes are daemon-saved house states. Capture is storage-only; apply can regroup rooms, change sources, volume and mute, and is undoable through action history.' }),
        el('div', { class: 'row' }, button('Refresh scenes', 'refresh', () => control.refresh(), s.loading || s.writing)));
      const name = el('input', { type: 'text', maxlength: 512, value: s.name,
        placeholder: 'Scene name', 'aria-label': 'Scene name', 'data-scene-focus': 'name',
        disabled: s.loading || s.writing, oninput: (e) => control.editName(e.target.value) });
      body.append(el('div', { class: 'scene-capture' }, name,
        button('Review capture', 'capture', () => control.review('capture'), !s.loaded || s.loading || s.writing || !!s.pending)));
      if (s.message) body.append(el('p', { role: 'status', text: s.message }));
      if (s.loading) body.append(el('p', { role: 'status', text: 'Reading scenes…' }));
      if (s.writing) body.append(el('p', { role: 'status', text: 'Checking and sending the reviewed scene change. Do not repeat it from another tab.' }));
      if (s.pending) {
        const p = s.pending;
        const title = p.action === 'capture'
          ? `${p.row ? 'Replace' : 'Create'} scene “${p.name}” from the house as it is now?`
          : p.action === 'apply' ? `Apply scene “${p.name}”?` : `Delete scene “${p.name}”?`;
        const review = el('section', { class: 'scene-review', 'aria-label': 'Review scene change' },
          el('h3', { text: title }));
        if (p.row) review.append(el('pre', { text: detail(p.row) }));
        review.append(el('p', { text: p.action === 'capture'
          ? 'Capture reads the current house only when confirmed and replaces the saved scene of this name if it still matches what you reviewed.'
          : p.action === 'delete'
            ? 'Deleting forgets only the saved scene. It does not change speakers and speaker undo cannot restore the saved definition.'
            : p.fadeSecs ? `Volumes ramp for ${p.fadeSecs} second(s). The definition is checked again before the speaker write, but another controller can still change the house concurrently.`
              : 'The definition is checked again before the speaker write, but another controller can still change the house concurrently.' }));
        review.append(el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel()),
          button(p.action === 'capture' ? 'Capture scene' : p.action === 'apply' ? 'Apply scene' : 'Delete scene',
            'confirm', () => control.confirm(p), s.writing)));
        body.append(review);
      }
      if (s.loaded && !s.rows.length) body.append(el('p', { text: 'No saved scenes yet.' }));
      for (const row of s.rows) {
        const fade = el('input', { type: 'number', min: 0, max: 60, step: 1, value: s.fade,
          'aria-label': `Fade seconds for ${row.name}`, 'data-scene-focus': `fade-${row.name}`,
          disabled: s.loading || s.writing || !!s.pending, oninput: (e) => control.editFade(e.target.value) });
        body.append(el('article', { class: 'scene-card' },
          el('h3', { text: row.name }), el('pre', { text: detail(row) }),
          el('div', { class: 'scene-fade' }, el('label', { text: 'Apply fade (0–60 s)' }), fade),
          el('div', { class: 'row' },
            button('Review apply', `apply-${row.name}`, () => control.review('apply', row.name), s.loading || s.writing || !!s.pending),
            button('Review delete', `delete-${row.name}`, () => control.review('delete', row.name), s.loading || s.writing || !!s.pending))));
      }
      const next = focus && [...body.querySelectorAll('[data-scene-focus]')]
        .find((e) => e.getAttribute('data-scene-focus') === focus);
      if (next && !next.disabled) next.focus({ preventScroll: true });
    }
    node.addEventListener('toggle', () => { if (node.open) void control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, sceneOf, scenesOf, signature, detail, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = ScenesRemote;
else {
  Object.assign(ROUTES, ScenesRemote.routes);
  document.getElementById('scenes-section')?.before(ScenesRemote.panel({ call, el }).node);
}
