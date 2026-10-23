// Music routines compose the existing scene and durable schedule APIs.
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

  // A library handoff is music and an observed destination, never a schedule,
  // saved scene, sound setting, or authority to submit either setup request.
  function librarySourceOf(value) {
    if (!object(value) || Object.keys(value).some((key) => !['kind', 'title', 'subtitle', 'source_uri', 'favorite', 'household', 'group'].includes(key))
        || !['spotify', 'favorite'].includes(value.kind) || !text(value.title, 2048)
        || typeof value.subtitle !== 'string' || value.subtitle.length > 2048 || /[\x00-\x1f\x7f]/.test(value.subtitle)) {
      fail('The selected library music is incomplete. Choose it again in the library.');
    }
    let group = null, household = null;
    if (value.group != null) {
      if (!object(value.group) || Object.keys(value.group).some((key) => !['target', 'members'].includes(key))
          || !text(value.household, 128) || !text(value.group.target, 512)
          || !Array.isArray(value.group.members) || !value.group.members.length || value.group.members.length > 512
          || !value.group.members.every((room) => text(room, 512) && room.endsWith(`@${value.household}`))
          || !value.group.members.includes(value.group.target)
          || new Set(value.group.members).size !== value.group.members.length) fail('The library destination is incomplete. Choose its group again.');
      group = { target: value.group.target, members: sorted(value.group.members) };
      household = value.household;
    } else if (value.household != null) fail('The library household has no observed group.');
    if (value.kind === 'favorite') {
      if (!group || value.source_uri != null || typeof value.favorite !== 'string' || !/^FV:2\/\d+$/.test(value.favorite)) {
        fail('A library favorite needs its original household and favorite ID.');
      }
      return freeze({ kind: 'favorite', title: value.title, subtitle: value.subtitle, favorite: value.favorite,
        source_uri: null, group, household });
    }
    if (value.favorite != null) fail('Choose one music source.');
    const source = sourceOf({ source_kind: 'spotify', source_uri: value.source_uri });
    return freeze({ kind: 'spotify', title: value.title, subtitle: value.subtitle, source_uri: source.uri,
      favorite: null, group, household });
  }

  function libraryFavorite(selection, favorites, group) {
    if (selection?.kind !== 'favorite' || !group
        || !group.members.every((room) => room.endsWith(`@${selection.household}`))) return null;
    const found = favorites.find((f) => f.id === selection.favorite && f.title === selection.title
      && (f.description || '') === selection.subtitle);
    const key = (name) => name.replace(/[A-Z]/g, (c) => c.toLowerCase());
    return found && favorites.filter((f) => key(f.title) === key(found.title)).length === 1 ? found : null;
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

  // This is the composer's supported subset. The daemon validates the source
  // again and owns rendering, account linkage, volume policy and playback.
  function sourceOf(draft, favorite) {
    const kind = draft.source_kind ?? 'favorite';
    if (kind === 'favorite') {
      if (!favorite || favoritesOf([favorite]).length !== 1) fail('Choose a playable favorite.');
      return { kind: 'favorite', name: favorite.id };
    }
    if (!['spotify', 'stream'].includes(kind)) fail('Choose a favorite, Spotify source or radio stream.');
    if (typeof draft.source_uri !== 'string') fail('Enter the music source.');
    const uri = draft.source_uri.trim();
    if (!text(uri, 2048) || /[\s\x80-\x9f]/.test(uri) || new TextEncoder().encode(uri).length > 2048) {
      fail('Use one source URI of at most 2048 bytes, without whitespace or control characters.');
    }
    if (kind === 'spotify') {
      let parts;
      if (/^spotify:/i.test(uri)) {
        parts = uri.slice(8).split(':');
        if (parts.length === 4 && parts[0].toLowerCase() === 'user' && parts[2].toLowerCase() === 'playlist') {
          parts = ['playlist', parts[3]];
        }
      } else {
        // Parse the raw path, as the server does; URL path normalization must
        // not turn an unreviewed path, user-info or lookalike host into music.
        const link = /^https?:\/\/(?:open|play)\.spotify\.com\/([^?#]*)(?:[?#].*)?$/i.exec(uri);
        if (!link) fail('Paste a Spotify track, album, playlist, artist, episode or show URI, or its full open.spotify.com link. Short links are not expanded.');
        parts = link[1].split('/').filter(Boolean);
        if (parts[0]?.startsWith('intl-')) parts.shift();
        if (parts[0] === 'embed') parts.shift();
        if (parts.length === 4 && parts[0] === 'user' && parts[2] === 'playlist') parts = ['playlist', parts[3]];
      }
      if (parts.length !== 2 || !['track', 'album', 'playlist', 'artist', 'episode', 'show'].includes(parts[0].toLowerCase())
          || !/^[A-Za-z0-9]{22}$/.test(parts[1])) fail('Spotify routines need a track, album, playlist, artist, episode or show with its full 22-character ID.');
      return { kind: 'uri', uri: `spotify:${parts[0].toLowerCase()}:${parts[1]}` };
    }
    let parsed;
    try { parsed = new URL(uri); } catch (_) { fail('Enter a full HTTP or HTTPS radio stream URL.'); }
    if (!/^https?:\/\/[^/?#@]+(?:[/?#]|$)/i.test(uri) || !['http:', 'https:'].includes(parsed.protocol)
        || !parsed.hostname || parsed.hash || uri.includes('\\')) {
      fail('Use an HTTP or HTTPS stream URL without embedded credentials, a fragment or backslashes.');
    }
    if (['open.spotify.com', 'play.spotify.com', 'spotify.link'].includes(parsed.hostname)
        || parsed.hostname.endsWith('.spotify.link')) fail('Choose Spotify for Spotify links; short links must be opened separately.');
    return { kind: 'uri', uri };
  }

  function settingsOf(draft) {
    const raw = draft.volume ?? '', unmute = draft.unmute ?? false;
    if (typeof raw !== 'string' || (raw !== '' && (!/^\d{1,3}$/.test(raw) || !integer(Number(raw), 0, 100)))
        || typeof unmute !== 'boolean') fail('Volume must be a whole number from 0 to 100, or blank to keep current levels.');
    return { volume: raw === '' ? null : Number(raw), unmute };
  }

  function definitionOf(group, source, settings) {
    return { groups: [{ coordinator: group.target,
      members: group.members.filter((r) => r !== group.target), source: copy(source), playing: true }],
    volumes: settings.volume === null ? {} : Object.fromEntries(group.members.map((r) => [r, settings.volume])),
    mutes: settings.unmute ? Object.fromEntries(group.members.map((r) => [r, false])) : {} };
  }

  function planSource(plan) {
    return plan.source ?? { kind: 'favorite', name: plan.favorite.id };
  }

  function sourceLabel(plan) {
    const source = planSource(plan);
    return source.kind === 'favorite' ? `${plan.favorite.title} · ${plan.favorite.kind}`
      : source.uri.startsWith('spotify:') ? `Spotify ${source.uri.split(':')[1]} · ${source.uri}` : source.uri;
  }

  function planOf(group, favorite, draft, nonce) {
    if (!text(draft.label, 80) || !/^[a-f0-9]{32}$/.test(nonce)) fail('Give this routine a name; a secure browser identity is required.');
    const source = sourceOf(draft, favorite), settings = settingsOf(draft);
    // A private random name avoids replacing a user's existing named scene.
    const name = `Routine ${Array.from(draft.label.trim()).slice(0, 20).join('')} ${nonce}`;
    return freeze({ name, group: copy(group), favorite: source.kind === 'favorite' ? copy(favorite) : null,
      source, settings, when: whenOf(draft), definition: definitionOf(group, source, settings) });
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

  function sameSettings(a, b) {
    return object(a) && object(b) && same(sorted(Object.keys(a)), sorted(Object.keys(b)))
      && Object.keys(a).every((key) => a[key] === b[key]);
  }

  function sceneOf(s, p) {
    if (!object(s) || s.name !== p.name || !Number.isSafeInteger(s.saved)
        || !Array.isArray(s.groups) || s.groups.length !== 1
        || !sameSettings(s.volumes, p.definition.volumes)
        || !sameSettings(s.mutes, p.definition.mutes)) fail('The saved scene does not match the reviewed routine.');
    const g = s.groups[0], requested = planSource(p);
    if (!object(g) || g.coordinator !== p.group.target || !Array.isArray(g.members)
        || !same(sorted(g.members), p.group.members.filter((r) => r !== p.group.target))
        || g.playing !== true || !object(g.source) || g.source.kind !== requested.kind || g.source.mood != null) {
      fail('The saved source or grouping changed. Inspect the scene; no routine was added.');
    }
    const source = requested.kind === 'favorite'
      ? { kind: 'favorite', name: g.source.name, uri: g.source.uri } : { kind: 'uri', uri: g.source.uri };
    if (requested.kind === 'favorite' ? g.source.name !== p.favorite.title || !text(g.source.uri, 16384)
      : g.source.name != null || g.source.uri !== requested.uri) fail('The saved music source differs from the review. No routine was added.');
    // Retain the legacy evidence shape when inspecting a version-one journal.
    return freeze({ name: s.name, saved: s.saved, group: { coordinator: g.coordinator,
      members: sorted(g.members), source, playing: true },
    ...(p.settings ? { volumes: copy(s.volumes), mutes: copy(s.mutes) } : {}) });
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
    if (!object(value) || ![1, 2].includes(value.version) || !object(value.plan) || typeof value.sceneSent !== 'boolean'
        || typeof value.scheduleSent !== 'boolean' || !value.sceneSent) fail('The saved routine attempt is unreadable; inspect Scenes and Routines before clearing browser data.');
    const p = value.plan;
    if (!text(p.name, 128) || !validWhen(p.when) || !object(p.group) || !text(p.group.target)
        || !Array.isArray(p.group.members) || !p.group.members.length || p.group.members.length > 512
        || !p.group.members.every((m) => text(m)) || !p.group.members.includes(p.group.target)
        || new Set(p.group.members).size !== p.group.members.length || !text(p.group.label, 4096)
        || !same(p.group.members, sorted(p.group.members)) || !object(p.definition)) fail('The saved routine review is unreadable.');
    let source, settings;
    if (value.version === 1) {
      if (p.source != null || p.settings != null) fail('The saved routine version is inconsistent.');
      source = sourceOf({}, p.favorite); settings = { volume: null, unmute: false };
    } else {
      if (!object(p.source) || !object(p.settings) || typeof p.settings.unmute !== 'boolean'
          || (p.settings.volume !== null && !integer(p.settings.volume, 0, 100))) fail('The saved source or level settings are unreadable.');
      const kind = p.source.kind === 'favorite' ? 'favorite' : p.source.kind === 'uri'
        ? (typeof p.source.uri === 'string' && p.source.uri.startsWith('spotify:') ? 'spotify' : 'stream') : '';
      source = sourceOf({ source_kind: kind, source_uri: p.source.uri }, p.favorite);
      settings = { volume: p.settings.volume, unmute: p.settings.unmute };
      if (!same(source, p.source) || !same(settings, p.settings)
          || (kind !== 'favorite' && p.favorite !== null)) fail('The saved music source is inconsistent.');
    }
    if (!same(p.definition, definitionOf(p.group, source, settings))) fail('The saved routine definition is inconsistent.');
    return { version: value.version, plan: freeze(copy(p)), sceneSent: true, scheduleSent: value.scheduleSent,
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
      if (planSource(p).kind === 'favorite') {
        const favorite = (await listFavorites(p.group.target)).find((f) => f.id === p.favorite.id);
        if (!favorite || !same(favorite, p.favorite)) fail('The favorite changed. Refresh and choose it again.');
      }
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
      async choose(target, sourceKind = 'favorite') {
        if (state.busy || state.attempt || !state.open || state.phase === 'blocked') return;
        const group = state.groups.find((g) => g.target === target);
        const version = ++epoch;
        state.pending = null; state.group = group || null; state.favorites = []; state.message = '';
        if (!group || sourceKind !== 'favorite') { publish(); return; }
        state.busy = true; publish();
        try { const found = await listFavorites(target); if (present(version)) state.favorites = found; }
        catch (e) { if (present(version)) state.message = message(e); }
        finally { state.busy = false; publish(); }
      },
      resumeDraft() {
        if (disposed) return false;
        state.open = true; publish(); return true;
      },
      async importSource(payload, { keepGroup = false, current = () => true } = {}) {
        if (disposed || state.busy || state.pending || state.attempt || !state.open || !active() || state.phase === 'blocked') return null;
        const selection = librarySourceOf(payload);
        const previous = keepGroup && selection.kind === 'spotify' ? state.group : null;
        const expected = previous || selection.group;
        const version = ++epoch;
        state.busy = true; publish();
        try {
          const groups = groupsOf(await call('routineZones'));
          if (!present(version) || !current()) return null;
          const group = expected && groups.find((g) => g.target === expected.target && same(g.members, expected.members)) || null;
          const favorites = group && selection.kind === 'favorite' ? await listFavorites(group.target) : [];
          if (!present(version) || !current()) return null;
          const favorite = libraryFavorite(selection, favorites, group);
          state.groups = groups; state.group = group; state.favorites = favorites; state.phase = 'choosing';
          state.message = expected && !group ? 'The group shown in the library changed. Choose a current group below.'
            : selection.kind === 'favorite' && !favorite ? 'The selected favorite changed or its title is ambiguous. Choose a current favorite below.'
              : 'Music filled in from your library. Choose the timing and review before saving.';
          return freeze({ source_kind: selection.kind, source_uri: selection.source_uri || '', favorite: favorite?.id || '', selection });
        } catch (e) {
          if (present(version) && current()) state.message = message(e);
          return null;
        } finally { state.busy = false; publish(); }
      },
      review(draft) {
        if (state.busy || state.attempt || !state.open || !active() || state.phase === 'blocked') return;
        try {
          const favorite = state.favorites.find((f) => f.id === draft.favorite);
          if (!state.group) fail('Choose a current group.');
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
          state.attempt = { version: 2, plan: expected, sceneSent: true, scheduleSent: false, scene: null };
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
  return { routes, groupsOf, favoritesOf, librarySourceOf, libraryFavorite, whenOf, sourceOf, settingsOf, sourceLabel,
    planOf, sceneOf, scheduleOf, journalOf, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = FavoriteRoutines;
