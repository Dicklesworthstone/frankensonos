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
      Object.entries(scene.volumes).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0),
      Object.entries(scene.mutes).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0),
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

  // SceneApplyDto omits empty optional lists. Missing is valid; null or a
  // malformed list is not evidence that all steps succeeded.
  function appliedOf(value, name) {
    const list = (key, valid) => {
      const items = Object.hasOwn(value, key) ? value[key] : [];
      if (!Array.isArray(items) || items.length > 4096 || !items.every(valid)) {
        throw new Error(`The scene ${key} report is unreadable.`);
      }
      return Object.freeze(items.map((item) => plain(item) ? Object.freeze({ ...item }) : item));
    };
    const words = (v) => text(v, 8192);
    if (!plain(value) || value.scene !== name || !words(value.done)
        || typeof value.complete !== 'boolean' || typeof value.changed !== 'boolean'
        || !Array.isArray(value.steps)) throw new Error('The apply reply is unreadable.');
    const steps = list('steps', words);
    const failed = list('failed', (v) => plain(v) && words(v.step) && words(v.error));
    const skipped = list('skipped', words);
    const notes = list('notes', (v) => plain(v) && text(v.code) && words(v.detail));
    if (value.complete !== (failed.length === 0) || value.changed !== (steps.length > 0)) {
      throw new Error('The apply reply has contradictory outcomes.');
    }
    return Object.freeze({ scene: value.scene, done: value.done, changed: value.changed,
      complete: value.complete, steps, failed, skipped, notes });
  }

  function model({ call, changed = () => {}, applied = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, rows: [], loaded: false, loading: false, writing: false,
      pending: null, name: '', fade: '0', message: '', result: null };
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
      if (disposed || !state.open || state.loading || state.writing) return;
      ++epoch; state.pending = null; state.name = String(value ?? ''); publish();
    }
    function editFade(value) {
      if (disposed || !state.open || state.loading || state.writing) return;
      ++epoch; state.pending = null; state.fade = String(value ?? ''); publish();
    }

    function review(action, name = null) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || state.pending) return false;
      if (!['capture', 'apply', 'delete'].includes(action)) return false;
      const asked = action === 'capture' ? state.name.trim() : name;
      if (!text(asked, action === 'capture' ? 64 : 512)) return false;
      const row = state.rows.find((r) => r.name === asked) || null;
      if (action !== 'capture' && !row) return false;
      let fadeSecs = null;
      if (action === 'apply') {
        if (!/^(?:0|[1-9]\d?)$/.test(state.fade)) return false;
        const value = Number(state.fade);
        if (!integer(value, 0, 60)) return false;
        fadeSecs = value;
      }
      state.pending = Object.freeze({ action, name: asked, row, fadeSecs });
      publish();
      return true;
    }

    function cancel() {
      if (disposed || !state.open || state.loading || state.writing) return;
      ++epoch; state.pending = null; publish();
    }

    async function confirm(reviewed) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing
          || !reviewed || reviewed !== state.pending) return false;
      const version = ++epoch;
      state.pending = null;
      state.writing = true;
      state.message = '';
      state.result = null;
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
          if (!disposed) state.message = `Captured the current house as “${saved.name}”. Refresh before another scene change.`;
          return true;
        }
        if (!now || signature(now) !== signature(reviewed.row)) {
          throw new Error('That scene changed or was removed. Refresh and review it again.');
        }
        const exact = sceneOf(await call('scene', { params: { name: reviewed.name } }));
        if (!current(version)) return false;
        if (signature(exact) !== signature(reviewed.row)) {
          throw new Error('That scene changed while it was being checked. Nothing was sent.');
        }
        sent = true;
        if (reviewed.action === 'apply') {
          const result = appliedOf(await call('sceneApplyReviewed', {
            params: { name: reviewed.name },
            body: reviewed.fadeSecs ? { fade_secs: reviewed.fadeSecs } : {},
          }), reviewed.name);
          // Once sent, retain its outcome even if the panel closed. Closing
          // cancels unsent work, not an already accepted speaker command.
          if (!disposed) {
            state.result = result;
            state.message = result.complete ? result.done
              : `Applied “${reviewed.name}” in part: ${result.failed.length} step(s) failed. Read the house before retrying anything.`;
          }
          return result.complete;
        }
        const result = await call('sceneDelete', { params: { name: reviewed.name }, body: {} });
        if (!plain(result) || result.deleted !== reviewed.name || !text(result.done, 8192)) {
          throw new Error('The delete reply did not confirm the reviewed scene.');
        }
        if (!disposed) state.message = `Deleted scene “${reviewed.name}”. Refresh before another scene change.`;
        return true;
      } catch (e) {
        if (current(version) || (sent && !disposed)) state.message = `${errorText(e)}${sent ? ' The outcome may be uncertain. The request was not repeated.' : ''} Refresh before another change.`;
        return false;
      } finally {
        state.writing = false;
        state.loaded = false;
        publish();
        // Reconcile playback once, even after an uncertain apply. A failed
        // UI refresh must never change the mutation result or repeat it.
        if (sent && reviewed.action === 'apply' && !disposed) {
          void Promise.resolve().then(applied).catch(() => {});
        }
      }
    }

    return { state, refresh, editName, editFade, review, cancel, confirm,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  function panel({ call, el, applied }) {
    const body = el('div', { class: 'scene-manager-body' });
    const node = el('details', { class: 'scene-manager scenes' },
      el('summary', { text: 'Manage scenes' }), body);
    const control = model({ call, changed: render, applied });
    const button = (label, key, work, disabled = false) => el('button', {
      type: 'button', text: label, 'data-scene-focus': key, disabled, onclick: work,
    });
    let reviewFocus = null;
    function render(s) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-scene-focus') : null;
      const caret = focus === 'name' ? active.selectionStart : null;
      if (s.pending && focus && focus !== 'cancel') reviewFocus = focus;
      body.replaceChildren();
      body.setAttribute('aria-busy', String(s.loading || s.writing));
      body.append(el('p', { text: 'Scenes are daemon-saved house states. Capture is storage-only; apply can regroup rooms and change sources, volume and mute. Action history can attempt undo, but cannot reconstruct replaced queues.' }),
        el('div', { class: 'row' }, button('Refresh scenes', 'refresh', () => control.refresh(), s.loading || s.writing)));
      const name = el('input', { type: 'text', maxlength: 128, value: s.name,
        placeholder: 'Scene name', 'aria-label': 'Scene name', 'data-scene-focus': 'name',
        disabled: s.loading || s.writing, oninput: (e) => control.editName(e.target.value) });
      body.append(el('div', { class: 'scene-capture' }, name,
        button('Review capture', 'capture', () => control.review('capture'), !s.loaded || s.loading || s.writing || !!s.pending)));
      if (s.message) body.append(el('p', { role: 'status', text: s.message }));
      if (s.result) {
        const result = s.result;
        const report = el('section', { class: 'scene-card', 'aria-label': 'Last scene application' },
          el('h3', { text: result.complete ? 'Application completed' : 'Application completed in part' }));
        report.append(el('p', { text: result.done }));
        for (const step of result.steps) report.append(el('p', { text: `Done: ${step}` }));
        for (const failure of result.failed) report.append(el('p', { text: `Failed: ${failure.step} — ${failure.error}` }));
        for (const skipped of result.skipped) report.append(el('p', { text: `Skipped: ${skipped}` }));
        for (const note of result.notes) report.append(el('p', { text: `${note.code}: ${note.detail}` }));
        body.append(report);
      }
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
          ? 'Capture reads the current house when confirmed and replaces this saved name. The definition is checked before sending, not locked; another client can still edit it between that read and the save.'
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
          disabled: !s.loaded || s.loading || s.writing || !!s.pending, oninput: (e) => control.editFade(e.target.value) });
        body.append(el('article', { class: 'scene-card' },
          el('h3', { text: row.name }), el('pre', { text: detail(row) }),
          el('div', { class: 'scene-fade' }, el('label', { text: 'Apply fade (0–60 s)' }), fade),
          el('div', { class: 'row' },
            button('Review apply', `apply-${row.name}`, () => control.review('apply', row.name), !s.loaded || s.loading || s.writing || !!s.pending),
            button('Review delete', `delete-${row.name}`, () => control.review('delete', row.name), !s.loaded || s.loading || s.writing || !!s.pending))));
      }
      // Keep keyboard review usable without stealing focus from elsewhere.
      const key = focus && (s.pending ? 'cancel' : focus === 'cancel' ? reviewFocus : focus === 'confirm' ? 'refresh' : focus);
      const next = key && [...body.querySelectorAll('[data-scene-focus]')]
        .find((e) => e.getAttribute('data-scene-focus') === key);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (key === 'name' && Number.isInteger(caret)) next.setSelectionRange(caret, caret);
      }
    }
    node.addEventListener('toggle', () => { if (node.open) void control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, sceneOf, scenesOf, signature, detail, appliedOf, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = ScenesRemote;
else {
  Object.assign(ROUTES, ScenesRemote.routes);
  document.getElementById('scenes-section')?.before(ScenesRemote.panel({ call, el, applied: () => loadZones() }).node);
}
