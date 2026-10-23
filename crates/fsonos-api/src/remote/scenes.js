// Saved scenes: inspect, capture, apply with fade, and remove through reviewed writes.
'use strict';

const ScenesRemote = (() => {
  const directionApi = () => typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const queueApi = () => typeof SceneQueue !== 'undefined' ? SceneQueue : require('./scene-queue.js');
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
    if (!plain(v) || !['keep', 'favorite', 'uri', 'dj', 'queue', 'queue_backup'].includes(v.kind)
        || (!['queue', 'queue_backup'].includes(v.kind) && v.queue != null) || (v.kind !== 'dj' && v.direction != null)) {
      throw new Error('A scene source is unreadable.');
    }
    for (const key of ['name', 'uri', 'mood']) if (v[key] != null && !text(v[key], 2048)) {
      throw new Error('A scene source field is unreadable.');
    }
    if (v.kind === 'favorite' && !v.name) throw new Error('A favorite source has no name.');
    if (v.kind === 'uri' && !v.uri) throw new Error('A URI source has no URI.');
    let queue;
    if (['queue', 'queue_backup'].includes(v.kind)) {
      queue = queueApi().read(v.queue);
      if ((v.kind === 'queue_backup') !== (queue.total != null)) throw new Error('The scene’s saved queue reference does not match its source kind.');
    }
    return Object.freeze({ kind: v.kind, name: v.name ?? null, uri: v.uri ?? null, mood: v.mood ?? null,
      ...(queue ? { queue } : {}),
      ...(v.direction != null ? { direction: directionApi().read(v.direction, v.mood) } : {}) });
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
      if (names.has(nameKey(scene.name))) throw new Error('The scene list contains duplicate names.');
      names.add(nameKey(scene.name));
      return scene;
    }));
  }

  function signature(scene) {
    return JSON.stringify([
      scene.name,
      scene.groups.map((g) => [g.coordinator, g.members, g.source.kind, g.source.name,
        g.source.uri, g.source.mood, g.playing,
        g.source.direction == null ? null : directionApi().signature(g.source.direction, g.source.mood),
        ['queue', 'queue_backup'].includes(g.source.kind) ? queueApi().signature(g.source.queue) : null]),
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
          : g.source.kind === 'dj' ? (g.source.direction != null ? directionApi().describe(g.source.direction, g.source.mood)
            : `DJ${g.source.mood ? ` (${g.source.mood})` : ''}`)
            : ['queue', 'queue_backup'].includes(g.source.kind) ? queueApi().describe(g.source.queue) : 'keep current source';
      lines.push(`${rooms}: ${src}${g.playing == null ? '' : g.playing ? '; playing' : '; paused'}`);
    }
    const volumes = Object.entries(scene.volumes).map(([room, level]) => `${room} ${level}`).join(', ');
    const mutes = Object.entries(scene.mutes).map(([room, mute]) => `${room} ${mute ? 'muted' : 'unmuted'}`).join(', ');
    if (volumes) lines.push(`Volumes: ${volumes}`);
    if (mutes) lines.push(`Mutes: ${mutes}`);
    return lines.join('\n') || 'This scene keeps all unspecified house state.';
  }

  // Scene names are case-insensitive in the daemon, using ASCII folding.
  const nameKey = (name) => name.replace(/[A-Z]/g, (c) => c.toLowerCase());
  const freeze = (value) => {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) {
      for (const child of Object.values(value)) freeze(child);
      Object.freeze(value);
    }
    return value;
  };

  function roomsOf(zones) {
    if (!Array.isArray(zones) || zones.length > 512) throw new Error('The room list is unreadable.');
    const rooms = [], seen = new Set();
    for (const z of zones) {
      if (!plain(z) || !text(z.household) || !text(z.coordinator_room)
          || !Array.isArray(z.members) || !z.members.length || z.members.length > 512
          || !z.members.every((r) => text(r)) || !z.members.includes(z.coordinator_room)) {
        throw new Error('The room list is incomplete.');
      }
      const shape = JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]);
      for (const room of z.members) {
        const target = `${room}@${z.household}`;
        if (!text(target) || seen.has(nameKey(target)) || rooms.length === 512) {
          throw new Error('The room list contains ambiguous or too many destinations.');
        }
        seen.add(nameKey(target));
        rooms.push(Object.freeze({ target, room, household: z.household, shape }));
      }
    }
    return Object.freeze(rooms);
  }

  const SOURCE_PAGE_SIZE = 6;
  const favoriteChoice = (f) => Object.freeze({ id: f.id, title: f.title, kind: f.kind,
    description: f.description ?? null, art_uri: f.art_uri ?? null });

  // A scene retains a favorite's title. An ID alone cannot make a duplicate
  // title safe to offer, because the scene engine resolves that title later.
  function sourceChoicesOf(kind, value) {
    if (kind === 'favorite') {
      if (!Array.isArray(value) || value.length > 10000 || !value.every((f) => plain(f)
          && text(f.id) && text(f.title) && ['track', 'stream', 'container', 'unplayable'].includes(f.kind)
          && (f.description == null || (typeof f.description === 'string' && f.description.length <= 8192))
          && (f.art_uri == null || (typeof f.art_uri === 'string' && f.art_uri.length <= 8192)))) {
        throw new Error('The favorite list is unreadable.');
      }
      if (new Set(value.map((f) => f.id)).size !== value.length) throw new Error('The favorite list repeats an ID.');
      const titles = new Map();
      for (const f of value) titles.set(nameKey(f.title), (titles.get(nameKey(f.title)) || 0) + 1);
      const playable = value.filter((f) => f.kind !== 'unplayable');
      return freeze({ items: playable.filter((f) => titles.get(nameKey(f.title)) === 1).map(favoriteChoice),
        ambiguous: playable.filter((f) => titles.get(nameKey(f.title)) > 1).length });
    }
    if (kind !== 'dj' || !plain(value) || !Array.isArray(value.moods) || value.moods.length > 512
        || !value.moods.every((m) => plain(m) && text(m.name, 128))) throw new Error('The DJ mood list is unreadable.');
    if (new Set(value.moods.map((m) => nameKey(m.name))).size !== value.moods.length) {
      throw new Error('The DJ mood list repeats a name.');
    }
    return freeze({ items: value.moods.map((m) => ({ id: m.name, title: m.name, kind: 'dj' })), ambiguous: 0 });
  }

  function sourcePage(choices) {
    const query = choices.query.trim().toLocaleLowerCase();
    const matches = choices.items.filter((item) => !query
      || [item.title, item.description || ''].join(' ').toLocaleLowerCase().includes(query));
    const page = Math.min(choices.page, Math.max(0, Math.ceil(matches.length / SOURCE_PAGE_SIZE) - 1));
    const offset = page * SOURCE_PAGE_SIZE;
    return { items: matches.slice(offset, offset + SOURCE_PAGE_SIZE), total: matches.length, page, offset,
      previous: page > 0, next: offset + SOURCE_PAGE_SIZE < matches.length };
  }

  // Match the public source normalizer without resolving links over a network.
  function sourceUri(raw) {
    const uri = raw.trim();
    if (!uri || new TextEncoder().encode(uri).length > 2048 || /[\s\x00-\x1f\x7f-\x9f]/u.test(uri)) {
      throw new Error('Use one music URI, at most 2048 bytes, without whitespace.');
    }
    const parts = /^([a-z][a-z0-9+.-]*):(.+)$/i.exec(uri);
    if (!parts) throw new Error('The music URI needs a scheme.');
    const scheme = parts[1].toLowerCase(), rest = parts[2];
    const spotify = (kind, id) => {
      kind = kind.toLowerCase();
      if (!['track', 'album', 'playlist', 'artist', 'episode', 'show'].includes(kind) || !/^[a-z0-9]{22}$/i.test(id)) {
        throw new Error('A scene plays a Spotify track, album, playlist, artist, episode or show with a complete 22-character ID.');
      }
      return `spotify:${kind}:${id}`;
    };
    if (scheme === 'spotify') {
      const p = rest.split(':');
      if (p.length === 2) return spotify(p[0], p[1]);
      if (p.length === 4 && p[0].toLowerCase() === 'user' && p[2].toLowerCase() === 'playlist') return spotify('playlist', p[3]);
      throw new Error('The Spotify URI is incomplete.');
    }
    if (['http', 'https'].includes(scheme) && rest.startsWith('//')) {
      const slash = rest.indexOf('/', 2);
      const host = rest.slice(2, slash < 0 ? undefined : slash).toLowerCase();
      if (host === 'spotify.link' || host.endsWith('.spotify.link')) throw new Error('Open the short Spotify link and paste its full destination.');
      if (['open.spotify.com', 'play.spotify.com'].includes(host)) {
        const p = (slash < 0 ? '' : rest.slice(slash + 1)).split(/[?#]/)[0].split('/').filter(Boolean);
        if (p[0]?.startsWith('intl-')) p.shift();
        if (p[0] === 'embed') p.shift();
        if (p.length === 2) return spotify(p[0], p[1]);
        if (p.length === 4 && p[0] === 'user' && p[2] === 'playlist') return spotify('playlist', p[3]);
        throw new Error('The Spotify link is incomplete.');
      }
    }
    return uri;
  }

  function definition(draft, rooms, name) {
    name = name.trim();
    if (!text(name, 64)) throw new Error('Use a scene name of 1 to 64 characters.');
    const byTarget = new Map(rooms.map((r) => [r.target, r]));
    const room = (target) => {
      if (!byTarget.has(target)) throw new Error('Choose a currently listed room for every target.');
      return byTarget.get(target);
    };
    const used = new Set();
    const groups = draft.groups.map((g) => {
      const leader = room(g.coordinator);
      for (const target of [g.coordinator, ...g.members]) {
        if (used.has(target)) throw new Error('Each room can belong to only one group in the scene.');
        if (room(target).household !== leader.household) throw new Error('Rooms in different households cannot share a group.');
        used.add(target);
      }
      if (!['keep', 'favorite', 'uri', 'dj'].includes(g.kind)
          || !['keep', 'play', 'pause'].includes(g.playing)
          || (g.kind !== 'keep' && g.playing === 'keep')) throw new Error('Choose a source and explicit playback setting.');
      const source = { kind: g.kind };
      if (g.kind === 'uri') source.uri = sourceUri(g.value);
      if (g.kind === 'favorite') {
        if (!text(g.value.trim())) throw new Error('Name a favorite by its exact title or ID.');
        source.name = g.value.trim();
      }
      if (g.kind === 'dj' && g.value.trim()) {
        if (!text(g.value.trim(), 128)) throw new Error('The DJ mood name is too long.');
        source.mood = g.value.trim();
      }
      if (g.kind === 'dj') source.direction = directionApi().fromDraft(g.direction ?? directionApi().draft(), source.mood);
      return { coordinator: leader.target, members: [...g.members], source,
        ...(g.playing === 'keep' ? {} : { playing: g.playing === 'play' }) };
    });
    const volumes = Object.create(null), mutes = Object.create(null);
    for (const [target, value] of Object.entries(draft.volumes)) {
      if (value === '') continue;
      room(target);
      if (!/^(?:0|[1-9]\d{0,2})$/.test(value) || Number(value) > 100) throw new Error('Room volumes must be whole numbers from 0 to 100, or blank to leave unchanged.');
      volumes[target] = Number(value);
    }
    for (const [target, value] of Object.entries(draft.mutes)) {
      if (value === 'keep') continue;
      room(target);
      if (!['muted', 'unmuted'].includes(value)) throw new Error('Choose muted, unmuted, or unchanged.');
      mutes[target] = value === 'muted';
    }
    // An empty object means CAPTURE to PUT /scenes/{name}, not an empty scene.
    if (!groups.length && !Object.keys(volumes).length && !Object.keys(mutes).length) {
      throw new Error('Define at least one group, volume or mute; an empty definition is not a custom scene.');
    }
    return { name, body: { groups, volumes, mutes } };
  }

  // The daemon enriches favorite sources with a renderer URI that the
  // read-only FavoriteDto deliberately does not expose. Compare every authored
  // field while leaving that server-learned field out of the save check.
  function authoredSignature(scene) {
    return signature({ ...scene, groups: scene.groups.map((g) => g.source.kind === 'favorite'
      ? { ...g, source: { ...g.source, uri: null } } : g) });
  }

  async function prepareDefinition(draft, rooms, name, call, active = () => true) {
    const prepared = definition(draft, rooms, name);
    const preview = structuredClone(prepared.body);
    const dependencies = [];
    for (let i = 0; i < prepared.body.groups.length; i++) {
      if (!active()) return null;
      const group = prepared.body.groups[i], source = group.source;
      if (source.kind === 'favorite') {
        const values = await call('favorites', { query: { zone: group.coordinator } });
        if (!active()) return null;
        if (!Array.isArray(values) || values.length > 10000
            || !values.every((f) => plain(f) && text(f.id) && text(f.title))) throw new Error('The favorite list is unreadable.');
        const ids = values.filter((f) => f.id === source.name);
        const matches = ids.length ? ids : values.filter((f) => nameKey(f.title) === nameKey(source.name));
        if (matches.length !== 1 || !['track', 'stream', 'container'].includes(matches[0].kind)) throw new Error('Choose one playable favorite by its exact title or unique ID.');
        const f = matches[0];
        // Saved scenes retain a favorite's title, not its ID. An ambiguous
        // title cannot later be applied reliably, even if an ID selected it.
        if (values.filter((v) => nameKey(v.title) === nameKey(f.title)).length !== 1) {
          throw new Error('That favorite title is ambiguous. Give it a unique title before saving a scene.');
        }
        if (draft.groups[i].choice
            && JSON.stringify(draft.groups[i].choice) !== JSON.stringify(favoriteChoice(f))) {
          throw new Error('The selected favorite changed. Refresh the choices and select it again.');
        }
        dependencies.push({ zone: group.coordinator, id: f.id, title: f.title, kind: f.kind,
          description: f.description ?? null, art_uri: f.art_uri ?? null });
        source.name = f.id;
        preview.groups[i].source = { kind: 'favorite', name: f.title };
      } else if (source.kind === 'dj' && source.mood) {
        const values = await call('moods', { query: { zone: group.coordinator } });
        if (!active()) return null;
        if (!plain(values) || !Array.isArray(values.moods) || values.moods.length > 512
            || !values.moods.every((m) => plain(m) && text(m.name, 128))) throw new Error('The DJ mood list is unreadable.');
        const matches = values.moods.filter((m) => nameKey(m.name) === nameKey(source.mood));
        if (matches.length !== 1) throw new Error('Choose a currently listed DJ mood, or leave it blank for automatic.');
        source.mood = preview.groups[i].source.mood = matches[0].name;
      }
    }
    const targets = new Set(prepared.body.groups.flatMap((g) => [g.coordinator, ...g.members])
      .concat(Object.keys(prepared.body.volumes), Object.keys(prepared.body.mutes)));
    return freeze({ ...prepared, dependencies, preview: sceneOf({ name: prepared.name, saved: 0, ...preview }),
      rooms: rooms.filter((r) => targets.has(r.target)) });
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
    let sourceRequest = 0;
    const state = { open: false, rows: [], loaded: false, loading: false, writing: false,
      pending: null, name: '', fade: '0', backupQueues: false, message: '', result: null, draft: null, rooms: null, saved: null,
      sourceChoices: Object.create(null) };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) {} } };
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      state.draft = state.rooms = null;
      state.sourceChoices = Object.create(null);
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
    function editBackupQueues(value) {
      if (disposed || !state.open || state.loading || state.writing || typeof value !== 'boolean') return;
      ++epoch; state.pending = null; state.backupQueues = value; publish();
    }

    function review(action, name = null) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || state.pending) return false;
      if (state.draft) return false;
      if (!['capture', 'apply', 'delete'].includes(action)) return false;
      const asked = action === 'capture' ? state.name.trim() : name;
      if (!text(asked, action === 'capture' ? 64 : 512)) return false;
      const row = state.rows.find((r) => nameKey(r.name) === nameKey(asked)) || null;
      if (action !== 'capture' && !row) return false;
      let fadeSecs = null;
      if (action === 'apply') {
        if (!/^(?:0|[1-9]\d?)$/.test(state.fade)) return false;
        const value = Number(state.fade);
        if (!integer(value, 0, 60)) return false;
        fadeSecs = value;
      }
      state.pending = Object.freeze({ action, name: asked, row, fadeSecs,
        ...(action === 'capture' ? { backupQueues: state.backupQueues } : {}) });
      publish();
      return true;
    }

    let groupId = 0;
    const editable = () => !disposed && state.open && state.loaded && !state.loading && !state.writing;
    async function beginDefinition() {
      if (!editable() || state.pending) return false;
      const version = ++epoch;
      state.loading = true;
      state.message = '';
      state.sourceChoices = Object.create(null);
      publish();
      try {
        const rooms = roomsOf(await call('zones'));
        if (!current(version)) return false;
        if (!rooms.length) throw new Error('No rooms are available for a custom scene.');
        state.rooms = rooms;
        state.draft = freeze({ groups: [], volumes: {}, mutes: {} });
        return true;
      } catch (e) {
        if (current(version)) state.message = errorText(e);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }
    function changeDraft(next) {
      ++epoch;
      state.pending = null;
      state.draft = freeze(next);
      for (const [id, choices] of Object.entries(state.sourceChoices)) {
        if (!next.groups.some((g) => String(g.id) === id && g.coordinator === choices.coordinator && g.kind === choices.kind)) {
          delete state.sourceChoices[id];
        }
      }
      state.message = '';
      publish();
      return true;
    }
    function addGroup() {
      if (!editable() || !state.draft || state.draft.groups.length >= 32) return false;
      return changeDraft({ ...state.draft, groups: [...state.draft.groups,
        { id: ++groupId, coordinator: '', members: [], kind: 'keep', value: '', playing: 'keep' }] });
    }
    function editGroup(group, key, value) {
      if (!editable() || !state.draft?.groups.includes(group)
          || !['coordinator', 'members', 'kind', 'value', 'playing'].includes(key)) return false;
      if (key === 'members' ? !Array.isArray(value) || !value.every((v) => typeof v === 'string') : typeof value !== 'string') return false;
      let next = { ...group, [key]: key === 'members' ? [...value] : value };
      if (['coordinator', 'kind', 'value'].includes(key)) delete next.choice;
      if (key === 'coordinator') {
        next = { ...next, members: [], kind: 'keep', value: '', playing: 'keep' };
        delete next.direction;
      }
      if (key === 'kind') {
        next = { ...next, value: '', playing: value === 'keep' ? 'keep' : 'play' };
        if (value === 'dj') next.direction = directionApi().draft();
        else delete next.direction;
      }
      return changeDraft({ ...state.draft, groups: state.draft.groups.map((g) => g === group ? next : g) });
    }

    function editDirection(group, field, value) {
      if (!editable() || !state.draft?.groups.includes(group) || group.kind !== 'dj') return false;
      try {
        const direction = directionApi().editDraft(group.direction, field, value);
        const next = { ...group, direction };
        return changeDraft({ ...state.draft, groups: state.draft.groups.map((g) => g === group ? next : g) });
      } catch (e) {
        state.message = errorText(e);
        publish();
        return false;
      }
    }

    function choicesFor(group) {
      const choices = state.sourceChoices[group.id];
      return choices?.coordinator === group.coordinator && choices.kind === group.kind ? choices : null;
    }
    async function loadSources(group) {
      if (!editable() || state.pending || !state.draft?.groups.includes(group)
          || !['favorite', 'dj'].includes(group.kind) || !state.rooms.some((r) => r.target === group.coordinator)) return false;
      const previous = choicesFor(group);
      if (previous?.phase === 'loading') return false;
      const request = ++sourceRequest;
      state.sourceChoices[group.id] = { coordinator: group.coordinator, kind: group.kind,
        request, phase: 'loading', items: [], ambiguous: 0, query: previous?.query || '', page: 0, error: '' };
      const owns = () => !disposed && state.open && state.sourceChoices[group.id]?.request === request
        && state.draft?.groups.some((g) => g.id === group.id && g.coordinator === group.coordinator && g.kind === group.kind);
      publish();
      try {
        const value = await call(group.kind === 'favorite' ? 'favorites' : 'moods', { query: { zone: group.coordinator } });
        if (!owns()) return false;
        const choices = sourceChoicesOf(group.kind, value);
        state.sourceChoices[group.id] = { ...state.sourceChoices[group.id], ...choices, phase: 'ready' };
        return true;
      } catch (e) {
        if (owns()) state.sourceChoices[group.id] = { ...state.sourceChoices[group.id], phase: 'error', error: errorText(e) };
        return false;
      } finally { if (owns()) publish(); }
    }
    function filterSources(group, query) {
      if (!editable() || state.pending || !state.draft?.groups.includes(group) || typeof query !== 'string') return false;
      const choices = choicesFor(group);
      if (!choices) return false;
      state.sourceChoices[group.id] = { ...choices, query: query.slice(0, 256), page: 0 };
      publish();
      return true;
    }
    function pageSources(group, direction) {
      if (!editable() || state.pending || !state.draft?.groups.includes(group) || ![-1, 1].includes(direction)) return false;
      const choices = choicesFor(group);
      if (choices?.phase !== 'ready') return false;
      const page = sourcePage(choices);
      if (direction === -1 ? !page.previous : !page.next) return false;
      state.sourceChoices[group.id] = { ...choices, page: page.page + direction };
      publish();
      return true;
    }
    function chooseSource(group, choice) {
      if (!editable() || state.pending || !state.draft?.groups.includes(group)
          || !['favorite', 'dj'].includes(group.kind)) return false;
      const choices = choicesFor(group);
      // Automatic DJ needs no catalog. Every named choice must be the exact
      // item displayed for this draft group's currently selected destination.
      if (choice === null ? group.kind !== 'dj' : choices?.phase !== 'ready' || !choices.items.includes(choice)) return false;
      const next = { ...group, value: choice?.id || '', choice };
      return changeDraft({ ...state.draft, groups: state.draft.groups.map((g) => g === group ? next : g) });
    }
    function removeGroup(group) {
      if (!editable() || !state.draft?.groups.includes(group)) return false;
      return changeDraft({ ...state.draft, groups: state.draft.groups.filter((g) => g !== group) });
    }
    function editRoom(target, field, value) {
      if (!editable() || !state.draft || !['volumes', 'mutes'].includes(field)
          || typeof value !== 'string' || !state.rooms.some((r) => r.target === target)) return false;
      return changeDraft({ ...state.draft, [field]: { ...state.draft[field], [target]: value } });
    }
    function discardDefinition() {
      if (disposed || state.writing) return false;
      ++epoch; state.pending = null; state.loading = false; state.draft = state.rooms = null;
      state.sourceChoices = Object.create(null);
      publish();
      return true;
    }
    async function reviewDefinition() {
      if (!editable() || !state.draft || state.pending) return false;
      const version = ++epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const prepared = await prepareDefinition(state.draft, state.rooms, state.name, call, () => current(version));
        if (!current(version) || !prepared) return false;
        state.pending = Object.freeze({ action: 'define', name: prepared.name, prepared,
          draft: state.draft, row: state.rows.find((r) => nameKey(r.name) === nameKey(prepared.name)) || null });
        return true;
      } catch (e) {
        if (current(version)) state.message = errorText(e);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
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
        const now = latest.find((r) => nameKey(r.name) === nameKey(reviewed.name)) || null;
        if (reviewed.action === 'capture' || reviewed.action === 'define') {
          if ((reviewed.row == null) !== (now == null)
              || (reviewed.row && signature(reviewed.row) !== signature(now))) {
            throw new Error('That scene changed while you reviewed the capture. Refresh and review again.');
          }
          let prepared = null;
          if (reviewed.action === 'define') {
            const rooms = roomsOf(await call('zones'));
            if (!current(version)) return false;
            for (const room of reviewed.prepared.rooms) {
              if (!rooms.some((r) => r.target === room.target && r.shape === room.shape)) {
                throw new Error('A selected room or its group changed. Reload the editor and review again.');
              }
            }
            prepared = await prepareDefinition(reviewed.draft, rooms, reviewed.name, call, () => current(version));
            if (!current(version) || !prepared) return false;
            if (signature(prepared.preview) !== signature(reviewed.prepared.preview)
                || JSON.stringify(prepared.body) !== JSON.stringify(reviewed.prepared.body)
                || JSON.stringify(prepared.dependencies) !== JSON.stringify(reviewed.prepared.dependencies)) {
              throw new Error('The selected source changed. Reload the editor and review again.');
            }
          }
          sent = true;
          const saved = sceneOf(await call('sceneSave', { params: { name: reviewed.name },
            body: prepared ? prepared.body : reviewed.backupQueues ? { backup_queues: true } : {} }));
          if (prepared && authoredSignature(saved) !== authoredSignature(prepared.preview)) {
            throw new Error('The reply did not confirm the reviewed custom scene definition.');
          }
          if (saved.name !== reviewed.name) throw new Error('The reply did not confirm the reviewed scene name.');
          if (!prepared && saved.groups.some((g) => g.source.kind === (reviewed.backupQueues ? 'queue' : 'queue_backup'))) {
            throw new Error('The reply did not confirm the reviewed queue capture option.');
          }
          if (!disposed) {
            state.saved = saved;
            state.draft = state.rooms = null;
            state.sourceChoices = Object.create(null);
            state.message = prepared ? `Saved custom scene “${saved.name}”. No playback was changed. Refresh and review it before applying.`
              : `Captured the current house as “${saved.name}”. Refresh before another scene change.`;
          }
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

    return { state, refresh, editName, editFade, editBackupQueues, review, cancel, confirm,
      beginDefinition, addGroup, editGroup, editDirection, removeGroup, editRoom, discardDefinition, reviewDefinition,
      choicesFor, loadSources, filterSources, pageSources, chooseSource,
      close() { state.open = false; invalidate(); state.draft = state.rooms = null; state.sourceChoices = Object.create(null); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  let panelId = 0;
  function panel({ call, el, applied }) {
    const identity = ++panelId, directionOpen = new Map();
    const body = el('div', { class: 'scene-manager-body' });
    const node = el('details', { class: 'scene-manager scenes' },
      el('summary', { text: 'Manage scenes' }), body);
    const control = model({ call, changed: render, applied });
    const button = (label, key, work, disabled = false) => el('button', {
      type: 'button', text: label, 'data-scene-focus': key, disabled, onclick: work,
    });
    const sourceKindLabel = (kind) => ({ track: 'Track', stream: 'Radio / stream', container: 'Album / playlist', dj: 'DJ mood' })[kind];
    function sourceChooser(g, index, busy) {
      const favorites = g.kind === 'favorite', noun = favorites ? 'favorites' : 'moods';
      const choices = control.choicesFor(g);
      const group = `Group ${index + 1}`;
      const section = el('section', { class: 'scene-music-picker', 'aria-label': `${group} music choices` });
      const selected = g.choice || choices?.items.find((item) => item.id === g.value);
      const selectedName = favorites ? selected?.title || g.value : g.value || 'Automatic DJ';
      if (selectedName) {
        const summary = el('div', { class: 'scene-source-selected', 'aria-label': `${group} selected source` },
          el('span', { class: 'scene-source-eyebrow', text: favorites ? 'Selected favorite' : 'Selected DJ mood' }),
          el('strong', { text: selectedName }));
        if (selected?.description) summary.append(el('span', { text: selected.description }));
        if (!favorites && !g.value) summary.append(el('span', {
          text: 'Use the house’s time-of-day program with the filters below. Empty settings clear temporary steering when this scene is applied.',
        }));
        if (choices?.phase === 'ready' && g.value && !choices.items.some((item) => item.id === g.value
            && (!g.choice || JSON.stringify(item) === JSON.stringify(g.choice)))) {
          summary.append(el('p', { class: 'scene-source-error', role: 'status',
            text: 'This selection changed or is no longer available. Choose a current source before saving.' }));
        }
        section.append(summary);
      }
      if (!favorites) {
        const automatic = button('Automatic DJ', `automatic-${g.id}`, () => control.chooseSource(g, null), busy);
        automatic.classList.add('scene-source-automatic');
        automatic.setAttribute('aria-pressed', String(!g.value));
        section.append(automatic);
      }
      const label = favorites ? 'Sonos favorites' : 'Named DJ moods';
      section.append(el('div', { class: 'scene-source-heading' },
        el('div', {}, el('strong', { text: label }), el('p', { text: `For ${g.coordinator}` })),
        button(choices?.phase === 'error' ? `Retry ${noun}` : choices ? `Refresh ${noun}` : `Load ${noun}`,
          `sources-reload-${g.id}`, () => control.loadSources(g), busy || choices?.phase === 'loading')));
      if (!choices || choices.phase === 'loading') {
        section.append(el('p', { role: 'status', text: choices ? `Reading ${noun} for this group…` : `Load this group’s ${noun} to choose its music.` }));
        return section;
      }
      if (choices.phase === 'error') {
        section.append(el('p', { class: 'scene-source-error', role: 'alert', text: choices.error }));
        return section;
      }
      if (choices.ambiguous) section.append(el('p', { class: 'scene-source-note',
        text: `${choices.ambiguous} favorite${choices.ambiguous === 1 ? ' has' : 's have'} a duplicate title. Rename ${choices.ambiguous === 1 ? 'it' : 'them'} in Sonos to use ${choices.ambiguous === 1 ? 'it' : 'them'} in a scene.` }));
      if (!choices.items.length) {
        section.append(el('p', { role: 'status', text: favorites
          ? 'No uniquely named playable favorites are available for this group. Add a favorite in its Sonos household, then refresh.'
          : 'No named moods are available. Automatic DJ is still available.' }));
        return section;
      }
      section.append(el('label', {}, favorites ? 'Find a favorite' : 'Find a mood', el('input', {
        type: 'search', value: choices.query, maxlength: 256, autocomplete: 'off',
        placeholder: favorites ? 'Title or description' : 'Mood name',
        'aria-label': `${group} find ${favorites ? 'a favorite' : 'a mood'}`,
        'data-scene-focus': `sources-search-${g.id}`, disabled: busy,
        oninput: (e) => control.filterSources(g, e.target.value),
      })));
      const page = sourcePage(choices);
      section.append(el('p', { class: 'scene-source-count', role: 'status', tabindex: -1,
        'data-scene-focus': `sources-count-${g.id}`, text: page.total
          ? `${page.offset + 1}–${page.offset + page.items.length} of ${page.total} ${noun}${choices.query.trim() ? ' matching your search' : ''}`
          : `No ${noun} match “${choices.query.trim()}”. Try another search.` }));
      const list = el('ul', { class: 'scene-source-options', 'aria-label': `${group} available ${noun}` });
      for (const item of page.items) {
        const pick = el('button', { type: 'button', class: 'scene-source-option', disabled: busy,
          'aria-pressed': String(g.value === item.id), 'data-scene-focus': `source-${g.id}-${item.id}`,
          onclick: () => control.chooseSource(g, item) },
          el('span', { class: 'scene-source-kind', text: sourceKindLabel(item.kind) }),
          el('strong', { text: item.title }),
          ...(item.description ? [el('span', { class: 'scene-source-description', text: item.description })] : []));
        list.append(el('li', {}, pick));
      }
      section.append(list);
      if (page.previous || page.next) {
        const move = (direction) => {
          if (control.pageSources(g, direction)) body.querySelector(`[data-scene-focus="sources-count-${g.id}"]`)?.focus({ preventScroll: true });
        };
        section.append(el('div', { class: 'row scene-source-pages' },
          button('Previous choices', `sources-previous-${g.id}`, () => move(-1), busy || !page.previous),
          button('Next choices', `sources-next-${g.id}`, () => move(1), busy || !page.next)));
      }
      return section;
    }
    function definitionForm(s) {
      const form = el('section', { class: 'scene-definition', 'aria-label': 'Custom scene definition' },
        el('h3', { text: 'Build a custom scene' }),
        el('p', { text: 'Only save a definition here; no speaker is changed. Declare exact groups, or leave groups empty for a volume/mute-only scene. Unlisted members of a declared group will detach when it is applied.' }));
      const busy = !s.loaded || s.loading || s.writing || !!s.pending;
      const select = (label, key, value, choices, work, unavailable = false) => {
        const node = el('select', { 'aria-label': label, 'data-scene-focus': key, disabled: busy || unavailable,
          onchange: (e) => work(e.target.value) });
        for (const [v, label] of choices) node.append(el('option', { value: v, text: label }));
        node.value = value;
        return el('label', {}, label, node);
      };
      for (const [i, g] of s.draft.groups.entries()) {
        const lead = s.rooms.find((r) => r.target === g.coordinator);
        const field = el('fieldset', {}, el('legend', { text: `Group ${i + 1}` }));
        field.append(select(`Group ${i + 1} coordinator`, `leader-${g.id}`, g.coordinator,
          [['', 'Choose a room…'], ...s.rooms.map((r) => [r.target, r.target])],
          (v) => control.editGroup(g, 'coordinator', v)));
        const members = el('div', { class: 'scene-members' });
        for (const r of s.rooms.filter((r) => lead && r.household === lead.household && r.target !== lead.target)) {
          members.append(el('label', {}, el('input', { type: 'checkbox', checked: g.members.includes(r.target),
            'aria-label': `Group ${i + 1} member ${r.target}`, 'data-scene-focus': `member-${g.id}-${r.target}`, disabled: busy,
            onchange: (e) => control.editGroup(g, 'members', e.target.checked
              ? [...g.members, r.target] : g.members.filter((v) => v !== r.target)) }), r.target));
        }
        field.append(members, select(`Group ${i + 1} source`, `source-${g.id}`, g.kind,
          [['keep', 'Keep the current source'], ['favorite', 'Sonos favorite'], ['uri', 'Music URI or Spotify link'], ['dj', 'DJ']],
          (v) => {
            if (control.editGroup(g, 'kind', v) && ['favorite', 'dj'].includes(v)) {
              void control.loadSources(control.state.draft.groups.find((next) => next.id === g.id));
            }
          }, !lead));
        if (g.kind === 'uri') {
          const label = 'Music URI or Spotify link';
          field.append(el('label', {}, label, el('input', { type: 'text', value: g.value,
            maxlength: 2048, 'aria-label': `Group ${i + 1} ${label}`, 'data-scene-focus': `value-${g.id}`, disabled: busy,
            oninput: (e) => control.editGroup(g, 'value', e.target.value) })));
        }
        if (lead && ['favorite', 'dj'].includes(g.kind)) field.append(sourceChooser(g, i, busy));
        if (lead && g.kind === 'dj') {
          const direction = directionApi().editor({
            draft: g.direction, change: (key, value) => control.editDirection(g, key, value),
            disabled: busy, el, idPrefix: `scene-direction-${identity}-${g.id}`,
            focusAttribute: 'data-scene-focus', open: directionOpen.get(g.id) || false,
            toggle: (open) => {
              if (control.state.draft?.groups.includes(g)) directionOpen.set(g.id, open);
            },
          });
          direction.setAttribute('data-scene-direction-id', String(g.id));
          direction.setAttribute('aria-label', `Group ${i + 1} DJ direction`);
          field.append(direction);
        }
        field.append(select(`Group ${i + 1} playback`, `playing-${g.id}`, g.playing,
          [...(g.kind === 'keep' ? [['keep', 'Leave playback unchanged']] : []), ['play', 'Playing'], ['pause', 'Paused']],
          (v) => control.editGroup(g, 'playing', v)),
          button('Remove group', `remove-group-${g.id}`, () => control.removeGroup(g), busy));
        form.append(field);
      }
      form.append(button('Add group', 'add-group', () => control.addGroup(), busy || s.draft.groups.length >= 32),
        el('h3', { text: 'Optional room settings' }),
        el('p', { text: 'Blank volume and unchanged mute leave those settings alone. Saved levels remain subject to the house policy when applied.' }));
      const settings = el('div', { class: 'scene-room-settings' });
      for (const room of s.rooms) {
        settings.append(el('fieldset', {}, el('legend', { text: room.target }),
          el('label', {}, 'Volume (blank = unchanged)', el('input', { type: 'number', min: 0, max: 100, step: 1,
            value: s.draft.volumes[room.target] ?? '', 'aria-label': `Volume for ${room.target}`,
            'data-scene-focus': `level-${room.target}`, disabled: busy,
            oninput: (e) => control.editRoom(room.target, 'volumes', e.target.value) })),
          select(`Mute for ${room.target}`, `mute-${room.target}`, s.draft.mutes[room.target] ?? 'keep',
            [['keep', 'Leave unchanged'], ['muted', 'Muted'], ['unmuted', 'Unmuted']],
            (v) => control.editRoom(room.target, 'mutes', v))));
      }
      form.append(settings, el('div', { class: 'row' },
        button('Cancel custom scene', 'discard-definition', () => control.discardDefinition(), s.writing),
        button('Review custom scene', 'review-definition', () => reviewCustom(), busy)));
      return form;
    }
    let reviewFocus = null;
    async function reviewCustom() {
      const active = document.activeElement;
      reviewFocus = active?.getAttribute('data-scene-focus');
      const reviewed = await control.reviewDefinition();
      if (reviewed && control.state.pending?.action === 'define'
          && (document.activeElement === active || document.activeElement === document.body)) {
        body.querySelector('.scene-review')?.scrollIntoView({ block: 'start', behavior: 'auto' });
        body.querySelector('[data-scene-focus="cancel"]')?.focus({ preventScroll: true });
      }
    }
    function render(s) {
      const active = document.activeElement;
      const focus = body.contains(active) ? active.getAttribute('data-scene-focus') : null;
      const textInput = (input) => ['text', 'search'].includes(input.type) || input.tagName === 'TEXTAREA';
      const caret = focus && textInput(active) ? [active.selectionStart, active.selectionEnd] : null;
      for (const editor of body.querySelectorAll('[data-scene-direction-id]')) {
        directionOpen.set(Number(editor.getAttribute('data-scene-direction-id')), editor.open);
      }
      for (const id of directionOpen.keys()) {
        if (!s.draft?.groups.some((g) => g.id === id && g.kind === 'dj')) directionOpen.delete(id);
      }
      if (s.pending && focus && focus !== 'cancel') reviewFocus = focus;
      body.replaceChildren();
      body.setAttribute('aria-busy', String(s.loading || s.writing));
      body.append(el('p', { text: 'Scenes are daemon-saved house states. Capture is storage-only; apply can regroup rooms and change sources, volume and mute. Action history can attempt undo, but cannot reconstruct replaced queues.' }),
        el('div', { class: 'row' }, button('Refresh scenes', 'refresh', () => control.refresh(), s.loading || s.writing),
          button('New custom scene', 'new-definition', () => control.beginDefinition(), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft)));
      const name = el('input', { type: 'text', maxlength: 128, value: s.name,
        placeholder: 'Scene name', 'aria-label': 'Scene name', 'data-scene-focus': 'name',
        disabled: s.loading || s.writing, oninput: (e) => control.editName(e.target.value) });
      const backup = el('input', { type: 'checkbox', checked: s.backupQueues,
        'data-scene-focus': 'backup-queues', disabled: s.loading || s.writing || !!s.draft,
        onchange: (e) => control.editBackupQueues(e.target.checked) });
      body.append(el('div', { class: 'scene-capture' }, name,
        button('Review capture', 'capture', () => control.review('capture'), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft),
        el('label', { class: 'scene-backup-option' }, backup, el('span', { text: 'Back up queue contents' }))));
      if (s.draft) body.append(definitionForm(s));
      if (s.saved) body.append(el('details', {}, el('summary', { text: `Last saved scene: ${s.saved.name}` }), el('pre', { text: detail(s.saved) })));
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
        const title = p.action === 'define' ? `${p.row ? 'Replace' : 'Create'} custom scene “${p.name}”?` : p.action === 'capture'
          ? `${p.row ? 'Replace' : 'Create'} scene “${p.name}” from the house as it is now?`
          : p.action === 'apply' ? `Apply scene “${p.name}”?` : `Delete scene “${p.name}”?`;
        const review = el('section', { class: 'scene-review', 'aria-label': 'Review scene change' },
          el('h3', { text: title }));
        if (p.prepared) review.append(el('pre', { text: detail(p.prepared.preview) }));
        if (p.action === 'capture') review.append(el('p', { text: p.backupQueues
          ? 'Save complete queues, up to 1,000 entries and 4 MiB per queue. Recall can rebuild them on their original players, replacing current queue contents. Capture itself leaves speakers untouched.'
          : 'Save queue bookmarks. Recall requires each original player’s queue to remain unchanged. Select “Back up queue contents” to save the tracks too.' }));
        if (p.row) review.append(el('details', {}, el('summary', { text: 'Existing saved definition' }), el('pre', { text: detail(p.row) })));
        review.append(el('p', { text: p.action === 'define'
          ? 'Saving does not play or regroup anything. Apply the saved scene separately. The daemon resolves favorites and moods by name; later edits can change them. This preview is not an atomic lock on another client’s edits.'
          : p.action === 'capture'
          ? 'Capture reads the current house when confirmed and replaces this saved name. The definition is checked before sending, not locked; another client can still edit it between that read and the save.'
          : p.action === 'delete'
            ? 'Deleting forgets only the saved scene. It does not change speakers and speaker undo cannot restore the saved definition.'
            : p.fadeSecs ? `Volumes ramp for ${p.fadeSecs} second(s). The definition is checked again before the speaker write, but another controller can still change the house concurrently.`
              : 'The definition is checked again before the speaker write, but another controller can still change the house concurrently.' }));
        review.append(el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel()),
          button(p.action === 'define' ? 'Save custom scene' : p.action === 'capture' ? 'Capture scene' : p.action === 'apply' ? 'Apply scene' : 'Delete scene',
            'confirm', () => control.confirm(p), s.writing)));
        body.append(review);
      }
      if (s.loaded && !s.rows.length) body.append(el('p', { text: 'No saved scenes yet.' }));
      for (const row of s.rows) {
        const fade = el('input', { type: 'number', min: 0, max: 60, step: 1, value: s.fade,
          'aria-label': `Fade seconds for ${row.name}`, 'data-scene-focus': `fade-${row.name}`,
          disabled: !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft, oninput: (e) => control.editFade(e.target.value) });
        body.append(el('article', { class: 'scene-card' },
          el('h3', { text: row.name }), el('pre', { text: detail(row) }),
          el('div', { class: 'scene-fade' }, el('label', { text: 'Apply fade (0–60 s)' }), fade),
          el('div', { class: 'row' },
            button('Review apply', `apply-${row.name}`, () => control.review('apply', row.name), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft),
            button('Review delete', `delete-${row.name}`, () => control.review('delete', row.name), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft))));
      }
      // Keep keyboard review usable without stealing focus from elsewhere.
      const key = focus && (s.pending ? 'cancel' : focus === 'cancel' ? reviewFocus : focus === 'confirm' ? 'refresh' : focus);
      const next = key && [...body.querySelectorAll('[data-scene-focus]')]
        .find((e) => e.getAttribute('data-scene-focus') === key);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if (textInput(next) && caret?.every(Number.isInteger)) next.setSelectionRange(...caret);
      }
    }
    node.addEventListener('toggle', () => { if (node.open) void control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, sceneOf, scenesOf, signature, detail, appliedOf, roomsOf, sourceUri, definition, prepareDefinition,
    sourceChoicesOf, sourcePage, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = ScenesRemote;
else {
  Object.assign(ROUTES, ScenesRemote.routes);
  document.getElementById('scenes-section')?.before(ScenesRemote.panel({ call, el, applied: () => loadZones() }).node);
}
