// Favorite routines compose the existing scene and durable schedule APIs.
// No browser alarm, playback call, automatic write retry or rollback.
'use strict';

const FavoriteRoutines = (() => {
  const routes = {
    routineZones: ['GET', '/zones'], routineFavorites: ['GET', '/favorites'],
    routineScenes: ['GET', '/scenes'], routineSaveScene: ['PUT', '/scenes/{name}'],
    routineSchedules: ['GET', '/schedules'], routineAdd: ['POST', '/schedules'],
  };
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const text = (v, max = 512) => typeof v === 'string' && v.trim() !== '' && v.length <= max && !/[\x00-\x1f\x7f]/.test(v);
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const fail = (message) => { throw new Error(message); };
  const copy = (v) => JSON.parse(JSON.stringify(v));
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const sorted = (a) => [...a].sort();
  const message = (e) => text(e?.message, 4096) ? e.message : 'The request was not confirmed.';
  const freeze = (v) => {
    if (object(v) || Array.isArray(v)) { Object.values(v).forEach(freeze); Object.freeze(v); }
    return v;
  };

  function groupsOf(rows) {
    if (!Array.isArray(rows) || rows.length > 512) fail('The group list is unreadable.');
    const seen = new Set();
    return rows.map((g) => {
      if (!object(g) || !text(g.household, 128) || !text(g.coordinator_room, 256)
          || !Array.isArray(g.members) || !g.members.length || g.members.length > 512
          || !g.members.every((r) => text(r, 256)) || !g.members.includes(g.coordinator_room)) {
        fail('The group membership is incomplete.');
      }
      const members = sorted(g.members.map((r) => `${r}@${g.household}`));
      for (const room of members) {
        if (seen.has(room)) fail('The group list contains ambiguous room membership.');
        seen.add(room);
      }
      return freeze({ target: `${g.coordinator_room}@${g.household}`, members,
        label: `${g.members.join(' + ')} (${g.household})` });
    });
  }

  function favoritesOf(rows) {
    if (!Array.isArray(rows) || rows.length > 10000) fail('The favorites list is unreadable.');
    const seen = new Set();
    return rows.map((f) => {
      if (!object(f) || !text(f.id, 256) || !text(f.title, 2048)
          || !['track', 'stream', 'container', 'unplayable'].includes(f.kind)
          || (f.description != null && (typeof f.description !== 'string' || f.description.length > 2048)) || seen.has(f.id)) {
        fail('A favorite is incomplete or has a duplicate identity.');
      }
      seen.add(f.id);
      return freeze({ id: f.id, title: f.title, kind: f.kind, description: f.description ?? null });
    }).filter((f) => f.kind !== 'unplayable');
  }

  function whenOf(draft) {
    if (draft.cadence === 'delay') {
      if (typeof draft.minutes !== 'string' || !/^\d+$/.test(draft.minutes)
          || !integer(Number(draft.minutes), 1, 10080)) fail('Choose a delay of 1 to 10080 whole minutes.');
      return `in ${Number(draft.minutes)}m`;
    }
    if (!['daily', 'weekdays', 'weekends'].includes(draft.cadence)
        || !/^(?:[01]\d|2[0-3]):[0-5]\d$/.test(draft.time)) fail('Choose the days and a house time in HH:MM format.');
    return `${draft.cadence} ${draft.time}`;
  }

  function validWhen(when) {
    return typeof when === 'string' && (/^(daily|weekdays|weekends) (?:[01]\d|2[0-3]):[0-5]\d$/.test(when)
      || (/^in \d+m$/.test(when) && integer(Number(when.slice(3, -1)), 1, 10080)));
  }

  function planOf(group, favorite, draft, nonce) {
    if (!text(draft.label, 80) || !/^[a-f0-9]{32}$/.test(nonce)) fail('Give this routine a name; a secure browser identity is required.');
    // A private random name avoids replacing a user's existing named scene.
    const name = `Routine ${Array.from(draft.label.trim()).slice(0, 20).join('')} ${nonce}`;
    return freeze({ name, group: copy(group), favorite: copy(favorite), when: whenOf(draft),
      definition: { groups: [{ coordinator: group.target,
        members: group.members.filter((r) => r !== group.target),
        source: { kind: 'favorite', name: favorite.id }, playing: true }], volumes: {}, mutes: {} } });
  }

  function sceneList(rows) {
    if (!Array.isArray(rows) || rows.length > 10000) fail('The scene list is unreadable.');
    const seen = new Set();
    for (const s of rows) {
      if (!object(s) || !text(s.name) || seen.has(s.name.toLowerCase())) fail('The scene list has missing or duplicate names.');
      seen.add(s.name.toLowerCase());
    }
    return rows;
  }

  function sceneOf(s, p) {
    if (!object(s) || s.name !== p.name || !Number.isSafeInteger(s.saved)
        || !Array.isArray(s.groups) || s.groups.length !== 1
        || !object(s.volumes) || Object.keys(s.volumes).length
        || !object(s.mutes) || Object.keys(s.mutes).length) fail('The saved scene does not match the reviewed routine.');
    const g = s.groups[0];
    if (!object(g) || g.coordinator !== p.group.target || !Array.isArray(g.members)
        || !same(sorted(g.members), p.group.members.filter((r) => r !== p.group.target))
        || g.playing !== true || !object(g.source) || g.source.kind !== 'favorite'
        || g.source.name !== p.favorite.title || !text(g.source.uri, 16384) || g.source.mood != null) {
      fail('The saved favorite or grouping changed. Inspect the scene; no routine was added.');
    }
    // Keep the resolved URI as evidence for the subsequent read; do not render it ourselves.
    return freeze({ name: s.name, saved: s.saved, group: { coordinator: g.coordinator,
      members: sorted(g.members), source: { kind: 'favorite', name: g.source.name, uri: g.source.uri }, playing: true } });
  }

  function scheduleOf(r, p) {
    if (!object(r) || !integer(r.id, 1, Number.MAX_SAFE_INTEGER) || r.scene !== p.name
        || r.action !== 'apply_scene' || typeof r.enabled !== 'boolean' || !text(r.creator)
        || !text(r.summary, 4096) || !text(r.when, 128)
        || ['zone', 'mood', 'volume', 'fade_secs'].some((k) => r[k] != null)) fail('The routine acknowledgement is unreadable.');
    if (p.when.startsWith('in ') ? !/^once \d{4}-\d\d-\d\dT/.test(r.when) || !Number.isFinite(Date.parse(r.when.slice(5)))
      : r.when !== p.when) fail('The routine time differs from the review.');
    for (const k of ['next', 'last_fired']) {
      if (r[k] != null && (!text(r[k], 128) || !Number.isFinite(Date.parse(r[k])))) fail('The routine time is unreadable.');
    }
    return freeze({ id: r.id, when: r.when, enabled: r.enabled, next: r.next ?? null, scene: r.scene });
  }

  function scheduleList(rows) {
    if (!Array.isArray(rows) || rows.length > 10000) fail('The schedule list is unreadable.');
    const seen = new Set();
    for (const r of rows) {
      if (!object(r) || !integer(r.id, 1, Number.MAX_SAFE_INTEGER) || !text(r.action, 128) || seen.has(r.id)) fail('The schedule list has missing or duplicate identities.');
      seen.add(r.id);
    }
    return rows;
  }

  function journalOf(value) {
    if (!object(value) || value.version !== 1 || !object(value.plan) || typeof value.sceneSent !== 'boolean'
        || typeof value.scheduleSent !== 'boolean' || !value.sceneSent) fail('The saved routine attempt is unreadable; inspect Scenes and Routines before clearing browser data.');
    const p = value.plan;
    if (!text(p.name, 128) || !validWhen(p.when) || !object(p.group) || !text(p.group.target)
        || !Array.isArray(p.group.members) || !p.group.members.length || p.group.members.length > 512
        || !p.group.members.every((m) => text(m)) || !p.group.members.includes(p.group.target)
        || new Set(p.group.members).size !== p.group.members.length || !text(p.group.label, 4096)
        || favoritesOf([p.favorite]).length !== 1 || !object(p.definition)) fail('The saved routine review is unreadable.');
    const expected = { groups: [{ coordinator: p.group.target,
      members: p.group.members.filter((r) => r !== p.group.target),
      source: { kind: 'favorite', name: p.favorite.id }, playing: true }], volumes: {}, mutes: {} };
    if (!same(p.definition, expected)) fail('The saved routine definition is inconsistent.');
    return { version: 1, plan: freeze(copy(p)), sceneSent: true, scheduleSent: value.scheduleSent,
      scene: value.scene ? copy(value.scene) : null };
  }

  function model({ call, changed = () => {}, nonce = () => Array.from(globalThis.crypto.getRandomValues(new Uint8Array(16)),
    (byte) => byte.toString(16).padStart(2, '0')).join(''),
    journal = { load: () => null, save: () => {}, clear: () => {} }, active = () => true }) {
    let epoch = 0, disposed = false;
    const state = { open: false, busy: false, groups: [], group: null, favorites: [],
      pending: null, attempt: null, phase: 'idle', message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Never turn a render failure into a retry. */ } } };
    const present = (version) => !disposed && state.open && epoch === version && active();
    const save = () => journal.save(copy(state.attempt));
    try {
      const stored = journal.load();
      if (stored != null) { state.attempt = journalOf(stored); state.phase = 'unknown'; state.message = 'A previous setup was sent. Inspect its saved state; it will not be repeated.'; }
    } catch (e) { state.phase = 'blocked'; state.message = message(e); }
    const listFavorites = (target) => call('routineFavorites', { query: { zone: target } }).then(favoritesOf);
    const scenes = () => call('routineScenes').then(sceneList);
    const schedules = () => call('routineSchedules').then(scheduleList);
    const lookupScene = async (p) => {
      const found = (await scenes()).filter((s) => s.name.toLowerCase() === p.name.toLowerCase());
      if (found.length !== 1) fail(`Saved scene ${p.name} was not found. No request was repeated.`);
      return sceneOf(found[0], p);
    };
    async function preflight(p) {
      const current = groupsOf(await call('routineZones')).find((g) => g.target === p.group.target);
      if (!current || !same(current.members, p.group.members)) fail('The group changed. Refresh and review the rooms again.');
      const favorite = (await listFavorites(p.group.target)).find((f) => f.id === p.favorite.id);
      if (!favorite || !same(favorite, p.favorite)) fail('The favorite changed. Refresh and choose it again.');
    }
    async function addSchedule(version) {
      const a = state.attempt, p = a.plan;
      if (a.scheduleSent) fail('This routine was already sent. Inspect its result instead of repeating it.');
      const observed = await lookupScene(p);
      if (!a.scene || !same(observed, a.scene)) fail('The saved scene was edited. No routine was added.');
      const matches = (await schedules()).filter((r) => r.scene === p.name);
      if (matches.length) fail('This scene already has a routine. Inspect Routines; no second one was added.');
      if (!present(version)) { state.phase = 'scene_ready'; state.message = 'The scene was saved. Reopen and inspect it before scheduling.'; return; }
      a.scheduleSent = true;
      // Persist intent before dispatch. A failed journal write must not send the POST.
      save();
      state.phase = 'schedule_sending'; publish();
      const row = await call('routineAdd', { body: { when: p.when, action: 'apply_scene', scene: p.name } });
      state.result = scheduleOf(row, p);
      state.phase = 'complete'; state.message = `Routine #${state.result.id} was acknowledged. Setup sent no playback command.`;
    }
    return {
      state,
      async refresh() {
        state.open = true;
        if (state.busy || state.attempt || state.phase === 'blocked') { publish(); return; }
        const version = ++epoch;
        state.busy = true; state.pending = null; state.group = null; state.favorites = []; publish();
        try {
          const groups = groupsOf(await call('routineZones'));
          if (present(version)) { state.groups = groups; state.phase = 'choosing'; state.message = ''; }
        } catch (e) { if (present(version)) { state.groups = []; state.message = message(e); } }
        finally { state.busy = false; publish(); }
      },
      async choose(target) {
        if (state.busy || state.attempt || !state.open || state.phase === 'blocked') return;
        const group = state.groups.find((g) => g.target === target);
        const version = ++epoch;
        state.pending = null; state.group = group || null; state.favorites = []; state.message = '';
        if (!group) { publish(); return; }
        state.busy = true; publish();
        try { const found = await listFavorites(target); if (present(version)) state.favorites = found; }
        catch (e) { if (present(version)) state.message = message(e); }
        finally { state.busy = false; publish(); }
      },
      review(draft) {
        if (state.busy || state.attempt || !state.open || !active() || state.phase === 'blocked') return;
        try {
          const favorite = state.favorites.find((f) => f.id === draft.favorite);
          if (!state.group || !favorite) fail('Choose a current group and playable favorite.');
          state.pending = planOf(state.group, favorite, draft, nonce()); state.message = '';
        } catch (e) { state.pending = null; state.message = message(e); }
        publish();
      },
      cancel() { if (!state.busy) { ++epoch; state.pending = null; publish(); } },
      async confirm(expected) {
        if (state.busy || !state.open || !active() || !expected || state.pending !== expected || state.attempt || state.phase === 'blocked') return;
        const version = ++epoch;
        state.busy = true; state.pending = null; state.phase = 'checking'; publish();
        try {
          await preflight(expected);
          if ((await scenes()).some((s) => s.name.toLowerCase() === expected.name.toLowerCase())) fail('The generated scene name already exists. Nothing was overwritten; review again.');
          if (!present(version)) return;
          state.attempt = { version: 1, plan: expected, sceneSent: true, scheduleSent: false, scene: null };
          save();
          state.phase = 'scene_sending'; publish();
          const saved = await call('routineSaveScene', { body: expected.definition, params: { name: expected.name } });
          state.attempt.scene = sceneOf(saved, expected);
          save();
          await addSchedule(version);
        } catch (e) {
          state.phase = state.attempt ? 'unknown' : 'choosing';
          state.message = `${message(e)}${state.attempt ? ' Inspect saved state before any further setup. No write was repeated.' : ''}`;
        } finally { state.busy = false; publish(); }
      },
      async recover() {
        state.open = true;
        if (state.busy || !state.attempt || state.phase === 'blocked') { publish(); return; }
        const version = ++epoch;
        state.busy = true; state.pending = null; publish();
        try {
          const a = state.attempt, scene = await lookupScene(a.plan);
          if (a.scene && !same(a.scene, scene)) fail('The saved scene has changed. Inspect Scenes and Routines; no request was repeated.');
          const matches = (await schedules()).filter((r) => r.scene === a.plan.name);
          if (!present(version)) return;
          if (matches.length > 1) fail('More than one routine names this scene. Inspect Routines; nothing was changed.');
          if (matches.length === 1) {
            state.result = scheduleOf(matches[0], a.plan); state.phase = 'complete';
            state.message = `Routine #${state.result.id} is present in the current read. This is observation, not proof that a lost request succeeded.`;
          } else if (a.scheduleSent) {
            state.phase = 'unknown'; state.message = 'No matching routine is visible yet. The submitted request may still finish. It will not be sent again.';
          } else {
            a.scene = scene; save(); state.phase = 'scene_ready';
            state.message = 'The reviewed scene is saved; no routine was submitted by this attempt. Review before creating the routine. The saved scene is not deleted automatically.';
          }
        } catch (e) { if (present(version)) { state.phase = 'unknown'; state.message = message(e); } }
        finally { state.busy = false; publish(); }
      },
      reviewContinue() {
        if (state.busy || !state.open || !active() || state.phase !== 'scene_ready' || state.attempt?.scheduleSent) return;
        state.pending = state.attempt.plan; publish();
      },
      async continue(expected) {
        if (state.busy || !state.open || !active() || state.phase !== 'scene_ready' || !expected
            || state.pending !== expected || state.attempt?.plan !== expected || state.attempt.scheduleSent) return;
        const version = ++epoch;
        state.pending = null; state.busy = true; publish();
        try { await preflight(expected); await addSchedule(version); }
        catch (e) { state.phase = 'unknown'; state.message = `${message(e)} No request was repeated.`; }
        finally { state.busy = false; publish(); }
      },
      async newRoutine() {
        if (state.busy || state.phase !== 'complete') return;
        try { journal.clear(); state.attempt = null; state.result = null; state.pending = null; state.phase = 'idle'; }
        catch (e) { state.message = message(e); publish(); return; }
        await this.refresh();
      },
      close() { ++epoch; state.open = false; state.pending = null; publish(); },
      dispose() { ++epoch; disposed = true; state.open = false; state.pending = null; },
    };
  }
  return { routes, groupsOf, favoritesOf, whenOf, planOf, sceneOf, scheduleOf, journalOf, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = FavoriteRoutines;
