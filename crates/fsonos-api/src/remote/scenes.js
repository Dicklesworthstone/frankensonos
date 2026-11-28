// Saved scenes: inspect, capture, edit complete typed definitions, apply and remove.
'use strict';

const ScenesRemote = (() => {
  const directionApi = () => typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const queueApi = () => typeof SceneQueue !== 'undefined' ? SceneQueue : require('./scene-queue.js');
  const sourceApi = () => typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const routes = {
    scene: ['GET', '/scenes/{name}'],
    sceneSave: ['PUT', '/scenes/{name}'],
    sceneApplyReviewed: ['POST', '/scenes/{name}/apply'],
    sceneDelete: ['DELETE', '/scenes/{name}'],
    sceneTargets: ['GET', '/rooms/resolve'],
    sonosPlaylists: ['GET', '/sonos/playlists'],
  };
  const text = (v, max = 512) => typeof v === 'string' && v.trim() !== '' && [...v].length <= max;
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const validRevision = v => text(v, 4096) && v !== 'absent' && !/[\u0000-\u001f\u007f]/.test(v);
  function revisionOf(scene) {
    if (scene == null) return 'absent';
    if (!validRevision(scene.revision)) throw new Error('The saved scene has no usable revision. Refresh before changing it.');
    return scene.revision;
  }
  const plain = (v) => v && typeof v === 'object' && !Array.isArray(v)
    && (Object.getPrototypeOf(v) === Object.prototype || Object.getPrototypeOf(v) === null);
  const errorText = (e) => text(e?.message, 2048)
    ? `${e.message}${text(e.hint, 2048) ? ` ${e.hint}` : ''}`
    : 'The request failed.';

  function sourceOf(v) {
    if (!plain(v) || !['keep', 'favorite', 'sonos_playlist', 'sonos_playlist_tracks', 'uri', 'tracks', 'dj', 'queue', 'queue_backup'].includes(v.kind)
        || (!['sonos_playlist', 'sonos_playlist_tracks'].includes(v.kind) && v.playlist != null)
        || (!['tracks', 'sonos_playlist_tracks'].includes(v.kind) && v.tracks != null)
        || v.selection != null
        || (!['queue', 'queue_backup'].includes(v.kind) && v.queue != null) || (v.kind !== 'dj' && v.direction != null)) {
      throw new Error('A scene source is unreadable.');
    }
    for (const key of ['name', 'uri', 'mood']) if (v[key] != null && !text(v[key], 2048)) {
      throw new Error('A scene source field is unreadable.');
    }
    if (v.kind === 'favorite' && !v.name) throw new Error('A favorite source has no name.');
    if (v.kind === 'uri' && !v.uri) throw new Error('A URI source has no URI.');
    let tracks;
    if (v.kind === 'tracks') {
      if (v.name != null || v.uri != null || v.mood != null) throw new Error('A track selection cannot also name another source.');
      tracks = queueApi().readTracks(v.tracks);
    }
    if (v.kind === 'sonos_playlist_tracks') tracks = queueApi().readNativeTracks(v.tracks);
    let playlist;
    if (['sonos_playlist', 'sonos_playlist_tracks'].includes(v.kind)) {
      const p = v.playlist;
      if (!plain(p) || !nativeId(p.id) || !text(p.title, 1024) || !text(p.household, 1024)
          || v.name !== p.id || v.uri != null || v.mood != null) throw new Error('The saved Sonos playlist identity is unreadable.');
      playlist = Object.freeze({ household: p.household, id: p.id, title: p.title });
    }
    let queue;
    if (['queue', 'queue_backup'].includes(v.kind)) {
      queue = queueApi().read(v.queue);
      if ((v.kind === 'queue_backup') !== (queue.total != null)) throw new Error('The scene’s saved queue reference does not match its source kind.');
    }
    return Object.freeze({ kind: v.kind, name: v.name ?? null, uri: v.uri ?? null, mood: v.mood ?? null,
      ...(queue ? { queue } : {}),
      ...(playlist ? { playlist } : {}),
      ...(tracks ? { tracks } : {}),
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
        || (v.revision != null && !validRevision(v.revision))
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
      revision: v.revision ?? null,
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
      scene.revision,
      scene.groups.map((g) => [g.coordinator, g.members, g.source.kind, g.source.name,
        g.source.uri, g.source.mood, g.playing,
        g.source.direction == null ? null : directionApi().signature(g.source.direction, g.source.mood),
        ['queue', 'queue_backup'].includes(g.source.kind) ? queueApi().signature(g.source.queue) : null,
        g.source.playlist ? [g.source.playlist.household, g.source.playlist.id, g.source.playlist.title] : null,
        g.source.kind === 'tracks' ? queueApi().tracksSignature(g.source.tracks)
          : g.source.kind === 'sonos_playlist_tracks' ? queueApi().nativeTracksSignature(g.source.tracks) : null]),
      Object.entries(scene.volumes).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0),
      Object.entries(scene.mutes).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0),
    ]);
  }

  function detail(scene) {
    const lines = [];
    for (const g of scene.groups) {
      const rooms = [g.coordinator, ...g.members].join(' + ');
      const src = g.source.kind === 'favorite' ? `favorite “${g.source.name}”`
        : g.source.kind === 'sonos_playlist' ? `saved Sonos playlist “${g.source.playlist.title}” (${g.source.playlist.id}, household ${g.source.playlist.household}; replaces the queue)`
        : g.source.kind === 'sonos_playlist_tracks' ? queueApi().describeNativeTracks(g.source.tracks, g.source.playlist)
        : g.source.kind === 'uri' ? g.source.uri
          : g.source.kind === 'tracks' ? queueApi().describeTracks(g.source.tracks)
          : g.source.kind === 'dj' ? (g.source.direction != null ? directionApi().describe(g.source.direction, g.source.mood)
            : `DJ${g.source.mood ? ` (${g.source.mood})` : ''}`)
            : ['queue', 'queue_backup'].includes(g.source.kind) ? queueApi().describe(g.source.queue) : 'keep current source';
      const playback = g.playing == null ? '' : g.playing ? '; playing'
        : ['tracks', 'sonos_playlist_tracks'].includes(g.source.kind) ? '; loads the queue without starting playback' : '; paused';
      lines.push(`${rooms}: ${src}${playback}`);
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
  const nativeId = (value) => typeof value === 'string' && value.length <= 1024 && /^SQ:\d+$/.test(value);
  const playlistChoice = (p) => Object.freeze({ id: p.id, title: p.title, kind: 'sonos_playlist',
    description: p.id, art_uri: p.art_uri ?? null });
  function resolvedRoom(value, target) {
    if (!plain(value) || value.target !== target || value.matched !== 'room'
        || !Array.isArray(value.rooms) || value.rooms.length !== 1) throw new Error('The scene room identity is unavailable.');
    const room = value.rooms[0];
    if (!plain(room) || !['room', 'household', 'player', 'coordinator'].every((key) => text(room[key], 1024))) {
      throw new Error('The scene room identity is unreadable.');
    }
    return Object.freeze({ room: room.room, household: room.household, player: room.player, coordinator: room.coordinator });
  }
  const favoriteChoice = (f) => Object.freeze({ id: f.id, title: f.title, kind: f.kind,
    description: f.description ?? null, art_uri: f.art_uri ?? null });

  // A scene retains a favorite's title. An ID alone cannot make a duplicate
  // title safe to offer, because the scene engine resolves that title later.
  function sourceChoicesOf(kind, value) {
    if (kind === 'sonos_playlist') {
      if (!Array.isArray(value) || value.length > 10000 || !value.every((p) => plain(p)
          && nativeId(p.id) && text(p.title, 1024) && !/[\x00-\x1f\x7f-\x9f]/u.test(p.title)
          && new TextEncoder().encode(p.title).length <= 1023
          && (p.art_uri == null || (typeof p.art_uri === 'string' && p.art_uri.length <= 8192)))) {
        throw new Error('The saved Sonos playlist catalog is unreadable.');
      }
      if (new Set(value.map((p) => p.id)).size !== value.length) throw new Error('The saved Sonos playlist catalog repeats an ID.');
      return freeze({ items: value.map(playlistChoice), ambiguous: 0 });
    }
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

  function trackSelection(raw) {
    if (typeof raw !== 'string' || raw.length > 32768) throw new Error('Paste up to 10 track or episode links, one per line.');
    const entries = raw.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
    return queueApi().readTracks(entries.map(line => {
      const divider = line.indexOf('|'), uri = divider < 0 ? line : line.slice(0, divider),
        title = divider < 0 ? '' : line.slice(divider + 1).trim();
      return { source_uri: sourceUri(uri), ...(title ? { title } : {}) };
    }));
  }

  function librarySelection(value) {
    if (value?.kind === 'sonos_playlist_tracks') return sourceApi().nativeSceneSelection(value);
    const group = value?.group;
    if (value?.kind !== 'tracks' || !plain(group) || !text(group.target, 1024)
        || !Array.isArray(group.members) || !group.members.length || group.members.length > 512
        || !group.members.every(room => text(room, 1024)) || new Set(group.members).size !== group.members.length
        || !group.members.includes(group.target)) throw new Error('Choose a listening group and its selected tracks again.');
    return freeze({ kind: 'tracks', tracks: queueApi().readTracks(value.tracks),
      group: { target: group.target, members: [...group.members].sort() } });
  }

  // Captured queues and URI metadata deliberately do not have an authored
  // round trip. Never open a partial editor that would discard those fields.
  function editingIssue(scene) {
    if (!validRevision(scene.revision)) return 'Refresh scenes from a daemon that provides editing revisions.';
    if (!text(scene.name, 64) || scene.name !== scene.name.trim()) return 'This saved name cannot be retained by the scene editor.';
    if (scene.groups.length > 32) return 'This scene has more than the editor’s 32 groups. Its saved definition is unchanged.';
    for (const group of scene.groups) {
      const kind = group.source.kind;
      if (['queue', 'queue_backup'].includes(kind)) return 'Editing is unavailable for captured queue bookmarks and backups: the public save API cannot preserve their captured contents and identity.';
      if (kind === 'uri') return 'Editing is unavailable for captured URI sources: the public scene view omits playback metadata that a replacement could lose.';
      if (kind === 'favorite') return 'Editing is unavailable for favorite scenes: replacing their definition would resolve the favorite again instead of preserving its saved source identity.';
      if (!['keep', 'dj', 'tracks', 'sonos_playlist'].includes(kind)) return 'This scene contains a source the editor cannot preserve.';
      if (kind !== 'keep' && group.playing == null) return 'This scene has an implicit source playback setting that the replacement API cannot retain exactly.';
    }
    if (!scene.groups.length && !Object.keys(scene.volumes).length && !Object.keys(scene.mutes).length) {
      return 'An empty saved definition cannot be edited: an empty save request captures the house instead.';
    }
    return '';
  }

  function draftFromScene(scene, references, nextId) {
    const issue = editingIssue(scene);
    if (issue) throw new Error(issue);
    const target = ref => {
      if (!Object.hasOwn(references, ref)) throw new Error('A saved room could not be resolved exactly. The scene was not changed.');
      return references[ref].target;
    };
    const used = new Set();
    const settings = (values, field, convert) => {
      const mapped = new Set();
      return Object.fromEntries(Object.entries(values).map(([ref, value]) => {
        const room = target(ref);
        if (mapped.has(room)) throw new Error(`The scene repeats a physical room in its ${field} settings. Its saved definition is unchanged.`);
        mapped.add(room);
        return [room, convert(value)];
      }));
    };
    return freeze({ references, groups: scene.groups.map(group => {
      const source = group.source;
      const coordinator = target(group.coordinator), members = group.members.map(target);
      for (const room of [coordinator, ...members]) {
        if (used.has(room)) throw new Error('The scene repeats a physical room in its groups. Its saved definition is unchanged.');
        used.add(room);
      }
      const draft = { id: nextId(), coordinator, members,
        roomReferences: { coordinator: group.coordinator,
          members: Object.fromEntries(group.members.map(ref => [target(ref), ref])) },
        kind: source.kind, value: source.kind === 'dj' ? source.mood || '' : source.name || '',
        playing: group.playing == null ? 'keep' : group.playing ? 'play' : 'pause' };
      if (source.kind === 'tracks') {
        draft.importedTracks = source.tracks;
        draft.value = source.tracks.map(track => track.source_uri
          + (track.title ? ` | ${track.title.replace(/[\r\n]+/g, ' ')}` : '')).join('\n');
      }
      if (source.kind === 'sonos_playlist') draft.savedPlaylist = source.playlist;
      if (source.kind === 'dj') {
        draft.originalDirection = source.direction ?? null;
        draft.directionEdited = [];
        const direction = { ...directionApi().draft() };
        for (const [key, value] of Object.entries(source.direction?.constraints || {})) {
          direction[key] = Array.isArray(value) ? value.join('\n') : typeof value === 'boolean' ? value : String(value);
        }
        if (source.direction?.for_secs != null && source.direction.for_secs % 60 === 0) {
          direction.minutes = String(source.direction.for_secs / 60);
        }
        draft.direction = direction;
      }
      return draft;
    }), volumes: settings(scene.volumes, 'volume', String),
    mutes: settings(scene.mutes, 'mute', value => value ? 'muted' : 'unmuted'),
    volumeReferences: Object.fromEntries(Object.keys(scene.volumes).map(ref => [target(ref), ref])),
    muteReferences: Object.fromEntries(Object.keys(scene.mutes).map(ref => [target(ref), ref])) });
  }

  function savedDirection(group, mood) {
    if (!Object.hasOwn(group, 'originalDirection')) return directionApi().fromDraft(group.direction ?? directionApi().draft(), mood);
    const edited = group.directionEdited || [];
    if (!edited.length) return group.originalDirection == null ? null : directionApi().read(group.originalDirection, mood);
    // Patch only fields the person changed. An unrelated change must preserve
    // exact seconds and individual names containing newlines or punctuation.
    const constraints = { ...(group.originalDirection?.constraints || {}) };
    const fields = edited.filter(key => key !== 'minutes');
    const parsed = directionApi().fromDraft({ ...directionApi().draft(),
      ...Object.fromEntries(fields.map(key => [key, group.direction[key]])) }, mood).constraints;
    for (const key of fields) {
      if (Object.hasOwn(parsed, key)) constraints[key] = parsed[key];
      else delete constraints[key];
    }
    const result = { constraints };
    if (group.originalDirection?.for_secs != null) result.for_secs = group.originalDirection.for_secs;
    if (edited.includes('minutes')) {
      const minutes = group.direction.minutes;
      if (minutes === '') delete result.for_secs;
      else {
        if (!/^\d+$/.test(minutes) || !integer(Number(minutes), 1, 10080)) {
          throw new Error('DJ duration must be 1–10080 whole minutes, or blank for until cleared.');
        }
        result.for_secs = Number(minutes) * 60;
      }
    }
    return directionApi().read(result, mood);
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
      if (!['keep', 'favorite', 'sonos_playlist', 'sonos_playlist_tracks', 'uri', 'tracks', 'dj'].includes(g.kind)
          || !['keep', 'play', 'pause'].includes(g.playing)
          || (g.kind !== 'keep' && g.playing === 'keep')) throw new Error('Choose a source and explicit playback setting.');
      const source = { kind: g.kind };
      if (g.kind === 'uri') source.uri = sourceUri(g.value);
      if (g.kind === 'tracks') source.tracks = g.importedTracks ? queueApi().readTracks(g.importedTracks) : trackSelection(g.value);
      if (g.kind === 'sonos_playlist_tracks') {
        const selected = sourceApi().nativeSceneSelection(g.nativeSelection);
        if (selected.native.group.household !== leader.household) throw new Error('A native selection must stay in its original household. Choose a scene room there.');
        Object.assign(source, sourceApi().nativeSceneSource(selected));
      }
      if (g.kind === 'favorite') {
        if (!text(g.value.trim())) throw new Error('Name a favorite by its exact title or ID.');
        source.name = g.value.trim();
      }
      if (g.kind === 'sonos_playlist') {
        if (!nativeId(g.value.trim())) throw new Error('Choose a saved Sonos playlist from this group’s household.');
        source.name = g.value.trim();
      }
      if (g.kind === 'dj' && g.value.trim()) {
        if (!text(g.value.trim(), 128)) throw new Error('The DJ mood name is too long.');
        source.mood = g.value.trim();
      }
      if (g.kind === 'dj') {
        const direction = savedDirection(g, source.mood);
        if (direction != null) source.direction = direction;
      }
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
    return signature({ ...scene, revision: null, groups: scene.groups.map((g) => g.source.kind === 'favorite'
      ? { ...g, source: { ...g.source, uri: null } } : ['sonos_playlist', 'sonos_playlist_tracks'].includes(g.source.kind)
        ? { ...g, source: { ...g.source, playlist: { ...g.source.playlist, household: null } } } : g) });
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
      } else if (source.kind === 'sonos_playlist') {
        const values = sourceChoicesOf('sonos_playlist', await call('sonosPlaylists', { query: { zone: group.coordinator } }));
        if (!active()) return null;
        const chosen = values.items.find((item) => item.id === source.name);
        if (!chosen || (draft.groups[i].choice && JSON.stringify(chosen) !== JSON.stringify(draft.groups[i].choice))) {
          throw new Error('The selected saved Sonos playlist changed. Refresh the choices and select it again.');
        }
        const identity = resolvedRoom(await call('sceneTargets', { query: { target: group.coordinator } }), group.coordinator);
        if (!active()) return null;
        const room = rooms.find((r) => r.target === group.coordinator);
        if (identity.room !== room.room || identity.household !== room.household) throw new Error('The playlist’s selected room changed.');
        const saved = draft.groups[i].savedPlaylist;
        if (saved && (saved.household !== identity.household || saved.id !== chosen.id || saved.title !== chosen.title)) {
          throw new Error('The saved playlist identity changed. Choose its replacement explicitly before saving the scene.');
        }
        dependencies.push({ kind: 'sonos_playlist', zone: group.coordinator, id: chosen.id, title: chosen.title, identity });
        preview.groups[i].source = { kind: 'sonos_playlist', name: chosen.id,
          playlist: { household: room.household, id: chosen.id, title: chosen.title } };
      } else if (source.kind === 'sonos_playlist_tracks') {
        const selection = draft.groups[i].nativeSelection;
        await sourceApi().verifyNativeSceneSelection({ selection, call,
          valid: () => { if (!active()) throw new Error('The native scene draft changed or closed.'); } });
        const identity = resolvedRoom(await call('sceneTargets', { query: { target: group.coordinator } }), group.coordinator);
        if (!active()) return null;
        const room = rooms.find(r => r.target === group.coordinator);
        if (identity.room !== room.room || identity.household !== room.household) throw new Error('The native scene destination changed.');
        dependencies.push({ kind: source.kind, zone: group.coordinator, identity, selection });
        preview.groups[i].source = sourceApi().nativeSceneSource(selection, true);
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
    if (draft.references) {
      const retained = new Set();
      const reference = (target, original) => {
        if (original == null || draft.references[original]?.target !== target) return target;
        retained.add(original);
        return original;
      };
      prepared.body.groups = prepared.body.groups.map((group, index) => {
        const saved = draft.groups[index].roomReferences;
        return { ...group, coordinator: reference(group.coordinator, saved?.coordinator),
          members: group.members.map(target => reference(target, saved?.members[target])) };
      });
      for (const [field, saved] of [['volumes', draft.volumeReferences], ['mutes', draft.muteReferences]]) {
        prepared.body[field] = Object.fromEntries(Object.entries(prepared.body[field])
          .map(([target, value]) => [reference(target, saved?.[target]), value]));
      }
      for (const original of retained) {
        const saved = draft.references[original];
        const identity = resolvedRoom(await call('sceneTargets', { query: { target: original } }), original);
        if (!active()) return null;
        if (JSON.stringify(identity) !== JSON.stringify(saved.identity)) {
          throw new Error('A saved room reference changed. Refresh scenes and open the edit again.');
        }
      }
    }
    return freeze({ ...prepared, dependencies, preview: sceneOf({ name: prepared.name, saved: 0, ...preview }),
      rooms: rooms.filter((r) => targets.has(r.target)) });
  }

  async function savedDefinition(saved, prepared, call) {
    const references = new Set(saved.groups.flatMap((g) => [g.coordinator, ...g.members])
      .concat(Object.keys(saved.volumes), Object.keys(saved.mutes)));
    const mapped = new Map();
    for (const target of references) {
      let room = prepared.rooms.find((r) => r.target === target);
      const native = prepared.dependencies.find((d) => ['sonos_playlist', 'sonos_playlist_tracks'].includes(d.kind) && d.zone === target);
      if (!room || native) {
        const identity = resolvedRoom(await call('sceneTargets', { query: { target } }), target);
        room = prepared.rooms.find((r) => r.room === identity.room && r.household === identity.household);
        if (!room) throw new Error('The saved scene names a different room.');
        const expected = prepared.dependencies.find((d) => ['sonos_playlist', 'sonos_playlist_tracks'].includes(d.kind) && d.zone === room.target);
        if (expected && JSON.stringify(identity) !== JSON.stringify(expected.identity)) {
          throw new Error('The saved playlist scene’s destination changed.');
        }
        for (const group of saved.groups.filter((g) => g.coordinator === target && ['sonos_playlist', 'sonos_playlist_tracks'].includes(g.source.kind))) {
          if (target !== `${identity.room}@${group.source.playlist.household}`) {
            throw new Error('The saved playlist scene did not confirm its pinned household.');
          }
        }
      }
      mapped.set(target, room.target);
    }
    const groupRooms = saved.groups.flatMap(group => [group.coordinator, ...group.members]).map(room => mapped.get(room));
    if (new Set(groupRooms).size !== groupRooms.length) throw new Error('The saved scene repeats a room identity in its groups.');
    const settings = values => {
      const entries = Object.entries(values).map(([room, value]) => [mapped.get(room), value]);
      if (new Set(entries.map(([room]) => room)).size !== entries.length) throw new Error('The saved scene repeats a room identity in its settings.');
      return Object.fromEntries(entries);
    };
    const comparable = { ...saved,
      groups: saved.groups.map((g) => ({ ...g, coordinator: mapped.get(g.coordinator), members: g.members.map((m) => mapped.get(m)) })),
      volumes: settings(saved.volumes), mutes: settings(saved.mutes) };
    if (authoredSignature(comparable) !== authoredSignature(prepared.preview)) {
      throw new Error('The reply did not confirm the reviewed custom scene definition.');
    }
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
      pending: null, name: '', fade: '0', backupQueues: false, message: '', result: null, draft: null, rooms: null, saved: null, editing: null,
      sourceChoices: Object.create(null) };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) {} } };
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      state.draft = state.rooms = state.editing = null;
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
      if (disposed || !state.open || state.loading || state.writing || state.editing) return;
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
      let expectedRevision = null;
      try { if (action !== 'apply') expectedRevision = revisionOf(row); }
      catch (error) { state.message = errorText(error); publish(); return false; }
      state.pending = Object.freeze({ action, name: asked, row, fadeSecs, expectedRevision,
        ...(action === 'capture' ? { backupQueues: state.backupQueues } : {}) });
      publish();
      return true;
    }

    let groupId = 0;
    const editable = () => !disposed && state.open && state.loaded && !state.loading && !state.writing;
    function offerSelection(value) {
      if (disposed || state.loading || state.writing || state.pending || state.draft) return false;
      let selection;
      try { selection = librarySelection(value); } catch (_) { return false; }
      state.open = true; invalidate();
      state.rooms = state.editing = null; state.name = ''; state.result = state.saved = null; state.sourceChoices = Object.create(null);
      const version = epoch;
      state.loading = true; state.message = 'Reading saved scenes and checking the selected listening group…'; publish();
      void (async () => {
        try {
          const [rows, zones] = await Promise.all([call('scenes'), call('zones')]);
          if (!current(version)) return;
          const rooms = roomsOf(zones), leader = rooms.find(room => room.target === selection.group.target);
          if (!leader || JSON.parse(leader.shape)[1] !== leader.room
              || JSON.stringify(rooms.filter(room => room.shape === leader.shape).map(room => room.target).sort()) !== JSON.stringify(selection.group.members)) {
            throw new Error('The selected listening group changed. Choose the tracks and group again from the library.');
          }
          state.rows = scenesOf(rows); state.loaded = true; state.rooms = rooms;
          if (selection.kind === 'sonos_playlist_tracks') {
            await sourceApi().verifyNativeSceneSelection({ selection, call,
              valid: () => { if (!current(version)) throw new Error('The native scene handoff closed.'); } });
            if (!current(version)) return;
          }
          state.draft = freeze({ groups: [{ id: ++groupId, coordinator: selection.group.target,
            members: selection.group.members.filter(room => room !== selection.group.target), kind: selection.kind,
            ...(selection.kind === 'sonos_playlist_tracks' ? { nativeSelection: selection } : { importedTracks: selection.tracks }),
            value: selection.kind === 'sonos_playlist_tracks' ? '' : selection.tracks.map(track => track.source_uri + (track.title ? ` | ${track.title.replace(/[\r\n]+/g, ' ')}` : '')).join('\n'),
            playing: 'play' }], volumes: {}, mutes: {} });
          state.message = 'Name this scene and review its ordered tracks, group and playback setting before saving.';
        } catch (error) { if (current(version)) state.message = errorText(error); }
        finally { if (current(version)) { state.loading = false; publish(); } }
      })();
      return true;
    }
    async function beginDefinition() {
      if (!editable() || state.pending || state.editing) return false;
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
        state.editing = null;
        state.draft = freeze({ groups: [], volumes: {}, mutes: {} });
        return true;
      } catch (e) {
        if (current(version)) state.message = errorText(e);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }
    async function beginEdit(name) {
      if (!editable() || state.pending || state.draft || typeof name !== 'string') return false;
      const row = state.rows.find(scene => nameKey(scene.name) === nameKey(name));
      if (!row) return false;
      const issue = editingIssue(row);
      if (issue) { state.message = issue; publish(); return false; }
      const version = ++epoch;
      state.loading = true; state.message = 'Reading the saved scene and its exact room identities…';
      state.sourceChoices = Object.create(null); state.result = state.saved = null; publish();
      try {
        const [value, zones] = await Promise.all([call('scene', { params: { name: row.name } }), call('zones')]);
        if (!current(version)) return false;
        const exact = sceneOf(value), rooms = roomsOf(zones);
        if (signature(exact) !== signature(row)) throw new Error('That scene changed. Refresh scenes before editing it.');
        const references = Object.create(null);
        const used = new Set(exact.groups.flatMap(group => [group.coordinator, ...group.members])
          .concat(Object.keys(exact.volumes), Object.keys(exact.mutes)));
        for (const reference of used) {
          const identity = resolvedRoom(await call('sceneTargets', { query: { target: reference } }), reference);
          if (!current(version)) return false;
          const room = rooms.find(room => room.room === identity.room && room.household === identity.household);
          if (!room) throw new Error('A saved room is unavailable. The scene was not changed.');
          references[reference] = { target: room.target, identity };
        }
        state.draft = draftFromScene(exact, references, () => ++groupId);
        state.rooms = rooms; state.name = exact.name; state.editing = exact;
        state.message = 'Edit the saved definition, then review it. Saving leaves playback unchanged; routines using this scene will use its new definition on their next run.';
        return true;
      } catch (error) {
        if (current(version)) state.message = errorText(error);
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
      if (['coordinator', 'kind', 'value'].includes(key)) delete next.importedTracks;
      if (['coordinator', 'kind', 'value'].includes(key)) delete next.savedPlaylist;
      if (['coordinator', 'kind'].includes(key)) {
        delete next.originalDirection; delete next.directionEdited;
      }
      if (key === 'coordinator') {
        delete next.roomReferences;
        next = group.kind === 'sonos_playlist_tracks' ? { ...next, members: [] }
          : { ...next, members: [], kind: 'keep', value: '', playing: 'keep' };
        delete next.direction;
      }
      if (key === 'kind') {
        if (value !== 'sonos_playlist_tracks') delete next.nativeSelection;
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
        const next = { ...group, direction, directionEdited: [...new Set([...(group.directionEdited || []), field])] };
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
          || !['favorite', 'sonos_playlist', 'dj'].includes(group.kind) || !state.rooms.some((r) => r.target === group.coordinator)) return false;
      const previous = choicesFor(group);
      if (previous?.phase === 'loading') return false;
      const request = ++sourceRequest;
      state.sourceChoices[group.id] = { coordinator: group.coordinator, kind: group.kind,
        request, phase: 'loading', items: [], ambiguous: 0, query: previous?.query || '', page: 0, error: '' };
      const owns = () => !disposed && state.open && state.sourceChoices[group.id]?.request === request
        && state.draft?.groups.some((g) => g.id === group.id && g.coordinator === group.coordinator && g.kind === group.kind);
      publish();
      try {
        const value = await call(group.kind === 'favorite' ? 'favorites' : group.kind === 'sonos_playlist' ? 'sonosPlaylists' : 'moods', { query: { zone: group.coordinator } });
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
          || !['favorite', 'sonos_playlist', 'dj'].includes(group.kind)) return false;
      const choices = choicesFor(group);
      // Automatic DJ needs no catalog. Every named choice must be the exact
      // item displayed for this draft group's currently selected destination.
      if (choice === null ? group.kind !== 'dj' : choices?.phase !== 'ready' || !choices.items.includes(choice)) return false;
      const next = { ...group, value: choice?.id || '', choice };
      delete next.savedPlaylist;
      if (group.kind === 'dj' && Object.hasOwn(group, 'originalDirection') && group.originalDirection == null) {
        delete next.originalDirection; delete next.directionEdited;
      }
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
      ++epoch; state.pending = null; state.loading = false; state.draft = state.rooms = state.editing = null;
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
        const row = state.rows.find((r) => nameKey(r.name) === nameKey(prepared.name)) || null;
        if (state.editing && (!row || signature(row) !== signature(state.editing) || prepared.name !== state.editing.name)) {
          throw new Error('The saved scene no longer matches this edit. Refresh scenes before editing it again.');
        }
        state.pending = Object.freeze({ action: 'define', name: prepared.name, prepared,
          draft: state.draft, row, expectedRevision: revisionOf(row), editing: state.editing != null });
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
            body: { ...(prepared ? prepared.body : reviewed.backupQueues ? { backup_queues: true } : {}),
              expected_revision: reviewed.expectedRevision } }));
          if (reviewed.editing && !validRevision(saved.revision)) {
            throw new Error('The reply did not include a valid revision for the edited scene.');
          }
          if (prepared) await savedDefinition(saved, prepared, call);
          if (saved.name !== reviewed.name) throw new Error('The reply did not confirm the reviewed scene name.');
          if (!prepared && saved.groups.some((g) => g.source.kind === (reviewed.backupQueues ? 'queue' : 'queue_backup'))) {
            throw new Error('The reply did not confirm the reviewed queue capture option.');
          }
          if (!disposed) {
            state.saved = saved;
            state.draft = state.rooms = state.editing = null;
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
        const result = await call('sceneDelete', { params: { name: reviewed.name },
          body: { expected_revision: reviewed.expectedRevision } });
        if (!plain(result) || result.deleted !== reviewed.name || !text(result.done, 8192)) {
          throw new Error('The delete reply did not confirm the reviewed scene.');
        }
        if (!disposed) state.message = `Deleted scene “${reviewed.name}”. Refresh before another scene change.`;
        return true;
      } catch (e) {
        if (current(version) || (sent && !disposed)) state.message = `${errorText(e)}${sent
          ? `${e?.code === 'CONFIG_CHANGED' ? '' : ' The outcome may be uncertain.'} The request was not repeated.`
          : ''} Refresh before another change.`;
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
      beginDefinition, beginEdit, offerSelection, addGroup, editGroup, editDirection, removeGroup, editRoom, discardDefinition, reviewDefinition,
      choicesFor, loadSources, filterSources, pageSources, chooseSource,
      close() { state.open = false; invalidate(); state.draft = state.rooms = state.editing = null; state.sourceChoices = Object.create(null); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  let panelId = 0;
  function panel({ call, el, applied }) {
    const identity = ++panelId, directionOpen = new Map(), doc = document;
    let skipOpenToggle = false;
    const body = el('div', { class: 'scene-manager-body' });
    const node = el('details', { class: 'scene-manager scenes' },
      el('summary', { text: 'Manage scenes' }), body);
    const control = model({ call, changed: render, applied });
    const button = (label, key, work, disabled = false) => el('button', {
      type: 'button', text: label, 'data-scene-focus': key, disabled, onclick: work,
    });
    const sourceKindLabel = (kind) => ({ track: 'Track', stream: 'Radio / stream', container: 'Album / playlist', sonos_playlist: 'Saved Sonos playlist', dj: 'DJ mood' })[kind];
    function sourceChooser(g, index, busy) {
      const favorites = g.kind === 'favorite', playlists = g.kind === 'sonos_playlist', dj = g.kind === 'dj';
      const noun = favorites ? 'favorites' : playlists ? 'saved playlists' : 'moods';
      const choices = control.choicesFor(g);
      const group = `Group ${index + 1}`;
      const section = el('section', { class: 'scene-music-picker', 'aria-label': `${group} music choices` });
      const selected = g.choice || choices?.items.find((item) => item.id === g.value);
      const selectedName = !dj ? selected?.title || g.savedPlaylist?.title || g.value : g.value || 'Automatic DJ';
      if (selectedName) {
        const summary = el('div', { class: 'scene-source-selected', 'aria-label': `${group} selected source` },
          el('span', { class: 'scene-source-eyebrow', text: favorites ? 'Selected favorite' : playlists ? 'Selected saved playlist' : 'Selected DJ mood' }),
          el('strong', { text: selectedName }));
        if (selected?.description) summary.append(el('span', { text: selected.description }));
        if (dj && !g.value) summary.append(el('span', {
          text: Object.hasOwn(g, 'originalDirection') && g.originalDirection == null && !g.directionEdited?.length
            ? 'Keep the scene’s existing DJ start behavior and the group’s current steering. Choose Automatic DJ to explicitly return to the program on recall.'
            : 'Use the house’s time-of-day program with the filters below. Empty settings clear temporary steering when this scene is applied.',
        }));
        if (choices?.phase === 'ready' && g.value && !choices.items.some((item) => item.id === g.value
            && (!g.choice || JSON.stringify(item) === JSON.stringify(g.choice)))) {
          summary.append(el('p', { class: 'scene-source-error', role: 'status',
            text: 'This selection changed or is no longer available. Choose a current source before saving.' }));
        }
        section.append(summary);
      }
      if (dj) {
        const automatic = button('Automatic DJ', `automatic-${g.id}`, () => control.chooseSource(g, null), busy);
        automatic.classList.add('scene-source-automatic');
        automatic.setAttribute('aria-pressed', String(!g.value));
        section.append(automatic);
      }
      const label = favorites ? 'Sonos favorites' : playlists ? 'Saved Sonos playlists' : 'Named DJ moods';
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
          : playlists ? 'No saved playlists are available in this household. Save a speaker queue as a playlist, then refresh.'
            : 'No named moods are available. Automatic DJ is still available.' }));
        return section;
      }
      section.append(el('label', {}, favorites ? 'Find a favorite' : playlists ? 'Find a saved playlist' : 'Find a mood', el('input', {
        type: 'search', value: choices.query, maxlength: 256, autocomplete: 'off',
        placeholder: favorites ? 'Title or description' : playlists ? 'Playlist title or SQ: id' : 'Mood name',
        'aria-label': `${group} find ${favorites ? 'a favorite' : playlists ? 'a saved playlist' : 'a mood'}`,
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
        el('h3', { text: s.editing ? `Edit scene “${s.editing.name}”` : 'Build a custom scene' }),
        el('p', { text: 'Only save a definition here; no speaker is changed. Declare exact groups, or leave groups empty for a volume/mute-only scene. Unlisted members of a declared group will detach when it is applied.' }));
      if (s.editing) form.append(el('p', { text: 'The saved name stays the same so existing routines keep using this scene. Changes take effect on its next recall or routine run. Cancel leaves the saved scene unchanged.' }));
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
          [['', 'Choose a room…'], ...s.rooms.filter(r => g.kind !== 'sonos_playlist_tracks'
            || r.household === g.nativeSelection.native.group.household).map((r) => [r.target, r.target])],
          (v) => control.editGroup(g, 'coordinator', v)));
        const members = el('div', { class: 'scene-members' });
        for (const r of s.rooms.filter((r) => lead && r.household === lead.household && r.target !== lead.target)) {
          members.append(el('label', {}, el('input', { type: 'checkbox', checked: g.members.includes(r.target),
            'aria-label': `Group ${i + 1} member ${r.target}`, 'data-scene-focus': `member-${g.id}-${r.target}`, disabled: busy,
            onchange: (e) => control.editGroup(g, 'members', e.target.checked
              ? [...g.members, r.target] : g.members.filter((v) => v !== r.target)) }), r.target));
        }
        field.append(members, select(`Group ${i + 1} source`, `source-${g.id}`, g.kind,
          [['keep', 'Keep the current source'], ['favorite', 'Sonos favorite'], ['sonos_playlist', 'Saved Sonos playlist'],
            ...(g.nativeSelection ? [['sonos_playlist_tracks', 'Selected native playlist occurrences']] : []),
            ['uri', 'Music URI or Spotify link'], ['tracks', 'Ordered tracks or episodes'], ['dj', 'DJ']],
          (v) => {
            if (control.editGroup(g, 'kind', v) && ['favorite', 'sonos_playlist', 'dj'].includes(v)) {
              void control.loadSources(control.state.draft.groups.find((next) => next.id === g.id));
            }
          }, !lead));
        if (g.kind === 'uri') {
          const label = 'Music URI or Spotify link';
          field.append(el('label', {}, label, el('input', { type: 'text', value: g.value,
            maxlength: 2048, 'aria-label': `Group ${i + 1} ${label}`, 'data-scene-focus': `value-${g.id}`, disabled: busy,
            oninput: (e) => control.editGroup(g, 'value', e.target.value) })));
        }
        if (g.kind === 'tracks') {
          const help = `scene-tracks-${identity}-${g.id}`;
          const input = el('textarea', { rows: 6,
            maxlength: 32768, 'aria-label': `Group ${i + 1} tracks in playback order`, 'aria-describedby': help,
            'data-scene-focus': `tracks-${g.id}`, disabled: busy,
            placeholder: 'Spotify track or episode URI or link | Optional title',
            oninput: (e) => control.editGroup(g, 'value', e.target.value) });
          input.value = g.value;
          field.append(el('label', {}, 'Tracks in playback order', input),
          el('p', { id: help, text: 'Add 1–10 Spotify tracks or episodes, one per line. Add | and a title if wanted. Order and repeated entries are kept. Recall replaces this group’s queue; Paused loads it without starting playback. Saving leaves speakers unchanged.' }));
          if (g.importedTracks?.some(track => /[\r\n]/.test(track.title || ''))) field.append(el('p', {
            text: 'Full library titles are retained in the saved selection and its review. Editing these lines uses the single-line titles shown here.' }));
        }
        if (g.kind === 'sonos_playlist_tracks') {
          const selection = g.nativeSelection, source = sourceApi().nativeSceneSource(selection, true);
          field.append(el('pre', { class: 'scene-source-detail', 'aria-label': `Group ${i + 1} native tracks in playback order`,
            text: queueApi().describeNativeTracks(source.tracks, source.playlist) }),
          el('p', { text: `Read from ${selection.native.group.target}. Its exact occurrences are checked again before saving. To change this sequence, cancel and select its tracks in Music again. Scene rooms can change within the same household; saving leaves speakers unchanged.` }));
        }
        if (lead && ['favorite', 'sonos_playlist', 'dj'].includes(g.kind)) field.append(sourceChooser(g, i, busy));
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
          if (Object.hasOwn(g, 'originalDirection')) {
            if (g.originalDirection == null && !g.directionEdited?.length) field.append(el('p', {
              text: 'This scene’s existing DJ start behavior is retained. Editing a direction field explicitly replaces its temporary steering on recall.' }));
            if (g.originalDirection?.for_secs % 60 && !g.directionEdited?.includes('minutes')) field.append(el('p', {
              text: `Its exact ${g.originalDirection.for_secs}-second direction duration is retained until you edit the duration field.` }));
            if (Object.entries(g.originalDirection?.constraints || {}).some(([key, values]) => Array.isArray(values)
                && !g.directionEdited?.includes(key) && values.some(value => typeof value === 'string' && /[\r\n]/.test(value)))) {
              field.append(el('p', { text: 'Full saved names, including embedded line breaks, are retained until you edit their field.' }));
            }
          }
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
        button(s.editing ? 'Cancel edit' : 'Cancel custom scene', 'discard-definition', () => control.discardDefinition(), s.writing),
        button(s.editing ? 'Review changes' : 'Review custom scene', 'review-definition', () => reviewCustom(), busy)));
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
        disabled: s.loading || s.writing || !!s.editing, oninput: (e) => control.editName(e.target.value) });
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
          ? 'Saving does not play or regroup anything. Apply the saved scene separately. The daemon checks that the saved definition still matches this review. Favorites, moods and the live house can change separately.'
          : p.action === 'capture'
          ? 'Capture reads the live house after confirmation. The daemon saves only if this name still has the reviewed definition, or remains unused for a new scene. Live grouping and playback can change during capture.'
          : p.action === 'delete'
            ? 'Deleting forgets only the reviewed saved definition and does not change speakers. If another controller changes it first, deletion is refused.'
            : p.fadeSecs ? `Volumes ramp for ${p.fadeSecs} second(s). The definition is checked again before the speaker write, but another controller can still change the house concurrently.`
              : 'The definition is checked again before the speaker write, but another controller can still change the house concurrently.' }));
        review.append(el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel()),
          button(p.action === 'define' ? 'Save custom scene' : p.action === 'capture' ? 'Capture scene' : p.action === 'apply' ? 'Apply scene' : 'Delete scene',
            'confirm', () => control.confirm(p), s.writing)));
        body.append(review);
      }
      if (s.loaded && !s.rows.length) body.append(el('p', { text: 'No saved scenes yet.' }));
      for (const row of s.rows) {
        const issue = editingIssue(row);
        const fade = el('input', { type: 'number', min: 0, max: 60, step: 1, value: s.fade,
          'aria-label': `Fade seconds for ${row.name}`, 'data-scene-focus': `fade-${row.name}`,
          disabled: !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft, oninput: (e) => control.editFade(e.target.value) });
        body.append(el('article', { class: 'scene-card' },
          el('h3', { text: row.name }), el('pre', { text: detail(row) }),
          el('div', { class: 'scene-fade' }, el('label', { text: 'Apply fade (0–60 s)' }), fade),
          el('div', { class: 'row' },
            button('Review apply', `apply-${row.name}`, () => control.review('apply', row.name), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft),
            button('Edit saved scene', `edit-${row.name}`, () => control.beginEdit(row.name), !!issue || !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft),
            button('Review delete', `delete-${row.name}`, () => control.review('delete', row.name), !s.loaded || s.loading || s.writing || !!s.pending || !!s.draft))));
        if (issue) body.append(el('p', { class: 'small-note', text: `${row.name}: ${issue}` }));
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
    function offerSelection(value) {
      if (doc.hidden || node.isConnected === false || control.state.loading || control.state.writing
          || control.state.pending || control.state.draft) return false;
      let selection;
      try { selection = librarySelection(value); } catch (_) { return false; }
      const workspace = node.closest?.('[data-studio-workspace]');
      if (workspace?.hidden) [...doc.querySelectorAll('#studio-nav [data-workspace]')]
        .find(button => button.dataset.workspace === workspace.dataset.studioWorkspace)?.click();
      if (node.closest?.('[hidden], [inert]') || !control.offerSelection(selection)) return false;
      if (!node.open) { skipOpenToggle = true; node.open = true; }
      node.scrollIntoView?.({ block: 'start', behavior: 'auto' });
      return true;
    }
    const fromLibrary = event => { if (offerSelection(event.detail)) event.preventDefault(); };
    doc.addEventListener?.('fsonos:scene-selection', fromLibrary);
    node.addEventListener('toggle', () => {
      if (node.open) { if (skipOpenToggle) skipOpenToggle = false; else void control.refresh(); }
      else { skipOpenToggle = false; control.close(); }
    });
    render(control.state);
    return { node, offerSelection, dispose() {
      doc.removeEventListener?.('fsonos:scene-selection', fromLibrary); control.dispose();
    } };
  }

  return { routes, sceneOf, scenesOf, signature, detail, appliedOf, roomsOf, sourceUri, trackSelection, librarySelection, editingIssue, definition, prepareDefinition,
    sourceChoicesOf, sourcePage, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = ScenesRemote;
else {
  Object.assign(ROUTES, ScenesRemote.routes);
  document.getElementById('scenes-section')?.before(ScenesRemote.panel({ call, el, applied: () => loadZones() }).node);
}
