// Reviewed collective control, using the daemon's real target resolver.
'use strict';
const RoomSetsRemote = (() => {
  const routes = { setRooms: ['GET', '/rooms'], resolveRooms: ['GET', '/rooms/resolve'],
    setPause: ['POST', '/pause'], setResume: ['POST', '/resume'],
    setMute: ['POST', '/mute'], setVolume: ['POST', '/volume'] };
  const actions = Object.freeze({ pause: 'Pause groups', resume: 'Resume groups',
    mute: 'Mute selected rooms', unmute: 'Unmute selected rooms',
    lower: 'Lower each room', raise: 'Raise each room', volume: 'Set each room’s volume' });
  const MAX_ROOMS = 512;
  const text = (v, max = 4096) => typeof v === 'string' && v.trim() !== '' && v.length <= max && !/[\p{Cc}]/u.test(v);
  const queryOf = v => {
    if (!text(v) || [...v.trim()].length > 128) throw new Error('Enter a room, alias, here, or everywhere (at most 128 characters).');
    return v.trim();
  };
  const errorText = e => [e?.message || 'The request failed.', e?.hint].filter(v => typeof v === 'string').join(' ');
  const roomKey = r => JSON.stringify([r.household, r.player]);
  const roomName = r => `${r.room}@${r.household}`;
  const identity = r => [r.household, r.room, r.player, r.coordinator];
  const order = (a, b) => roomKey(a) < roomKey(b) ? -1 : roomKey(a) > roomKey(b) ? 1 : 0;
  const transport = action => action === 'pause' || action === 'resume';

  function resolutionOf(value, target) {
    const bad = () => { throw new Error('The resolved room set is incomplete or contradictory. Resolve it again.'); };
    if (!value || value.target !== target || !['room', 'alias', 'here', 'everywhere', 'prefix'].includes(value.matched)
        || !Array.isArray(value.rooms) || !value.rooms.length || value.rooms.length > MAX_ROOMS
        || !Array.isArray(value.groups) || !value.groups.length || value.groups.length > MAX_ROOMS) bad();
    const parseRoom = r => {
      if (!r || !text(r.room) || !text(r.household) || !text(r.player, 256) || !text(r.coordinator, 256)) bad();
      return Object.freeze({ room: r.room, household: r.household, player: r.player, coordinator: r.coordinator });
    };
    const rooms = value.rooms.map(parseRoom).sort(order), selected = new Set(), named = new Set();
    for (const room of rooms) {
      if (selected.has(roomKey(room)) || named.has(roomName(room))) bad();
      selected.add(roomKey(room)); named.add(roomName(room));
    }
    const groupKeys = new Set(), allRooms = new Map(), allNames = new Set();
    const groups = value.groups.map(g => {
      if (!g || !text(g.household) || !text(g.coordinator, 256) || !text(g.coordinator_room)
          || !Array.isArray(g.members) || !g.members.length || g.members.length > MAX_ROOMS) bad();
      const key = JSON.stringify([g.household, g.coordinator]);
      if (groupKeys.has(key)) bad();
      groupKeys.add(key);
      const members = g.members.map(parseRoom).sort(order);
      if (!members.some(r => r.room === g.coordinator_room) || !members.some(r => selected.has(roomKey(r)))) bad();
      for (const room of members) {
        if (room.household !== g.household || room.coordinator !== g.coordinator
            || allRooms.has(roomKey(room)) || allNames.has(roomName(room))) bad();
        allRooms.set(roomKey(room), room); allNames.add(roomName(room));
      }
      return Object.freeze({ household: g.household, coordinator: g.coordinator, coordinator_room: g.coordinator_room,
        members: Object.freeze(members),
        shape: JSON.stringify([g.household, g.coordinator_room, members.map(r => r.room).sort()]) });
    }).sort((a, b) => a.shape < b.shape ? -1 : a.shape > b.shape ? 1 : 0);
    if (allRooms.size > MAX_ROOMS) bad();
    for (const room of rooms) if (JSON.stringify(identity(allRooms.get(roomKey(room)) || {})) !== JSON.stringify(identity(room))) bad();
    const signature = JSON.stringify([target, value.matched, rooms.map(identity),
      groups.map(g => [g.household, g.coordinator, g.coordinator_room, g.members.map(identity)])]);
    return Object.freeze({ target, matched: value.matched, rooms: Object.freeze(rooms), groups: Object.freeze(groups), signature });
  }

  // These are suggestions only. Never infer alias membership from this list.
  function suggestionsOf(value) {
    if (!Array.isArray(value) || value.length > MAX_ROOMS) throw new Error('The room suggestions are unreadable.');
    const choices = new Map([['everywhere', 'Everywhere · all households'], ['here', 'Here · your configured default']]);
    const seen = new Set();
    const add = (query, label) => { if ([...query].length <= 128) choices.set(query, label); };
    for (const room of value) {
      if (!room || !text(room.name) || !text(room.household) || !text(room.zone)
          || (room.aliases != null && (!Array.isArray(room.aliases) || room.aliases.length > 256 || !room.aliases.every(a => text(a, 128))))) {
        throw new Error('The room suggestions are incomplete.');
      }
      const key = JSON.stringify([room.household, room.name]);
      if (seen.has(key)) throw new Error('The room suggestions contain duplicates.');
      seen.add(key);
      add(`all@${room.household}`, `All rooms · ${room.household}`);
      add(`${room.name}@${room.household}`, `${room.name} · ${room.household}`);
      for (const alias of room.aliases || []) if (!choices.has(alias)) add(alias, `Alias · ${alias}`);
    }
    if (choices.size > 2048) throw new Error('Too many room suggestions; enter a target directly.');
    return Object.freeze([...choices].map(([query, label]) => Object.freeze({ query, label })));
  }

  function command(target, action, amount) {
    if (!Object.hasOwn(actions, action)) throw new Error('Choose a supported collective action.');
    const body = { zone: target };
    let route;
    if (action === 'pause' || action === 'resume') route = action === 'pause' ? 'setPause' : 'setResume';
    else if (action === 'mute' || action === 'unmute') { route = 'setMute'; body.mute = action === 'mute'; }
    else {
      if (typeof amount !== 'string' || !/^\d{1,3}$/.test(amount.trim())) throw new Error('Enter a whole-number volume or step.');
      const n = Number(amount.trim());
      if (n > 100 || (action !== 'volume' && n === 0)) throw new Error('Use volume 0–100, or a non-zero step 1–100.');
      route = 'setVolume'; body.group = false;
      if (action === 'volume') body.volume = n; else body.delta = action === 'lower' ? -n : n;
    }
    return Object.freeze({ route, body: Object.freeze(body), action, amount });
  }

  function resumeState(value, group) {
    const z = value?.zone, track = value?.track;
    if (!z || z.household !== group.household || z.coordinator_room !== group.coordinator_room
        || !Array.isArray(z.members) || !z.members.every(v => text(v))
        || JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]) !== group.shape
        || !['playing', 'paused', 'stopped'].includes(value.transport_state)
        || !track || !text(track.uri, 65536)
        || (track.queue_position != null && (!Number.isInteger(track.queue_position) || track.queue_position < 1 || track.queue_position > 4294967295))) {
      throw new Error('A group has no confirmed resumable source, or changed while it was read. Review it again.');
    }
    return Object.freeze({ group: group.shape, label: `${group.coordinator_room} (${group.household}): ${text(track.title) ? track.title : track.uri} · ${value.transport_state}`,
      signature: JSON.stringify([group.shape, value.transport_state, track.uri, track.queue_position ?? null]) });
  }

  function outcomeOf(value) {
    if (!value || !text(value.done, 65536) || typeof value.changed !== 'boolean'
        || (value.notes != null && (!Array.isArray(value.notes) || value.notes.length > 2048
          || value.notes.some(n => !n || !text(n.code, 256) || !text(n.detail, 65536))))) {
      throw new Error('The daemon did not return a complete collective-action result.');
    }
    const notes = (value.notes || []).map(n => Object.freeze({ code: n.code, detail: n.detail }));
    return Object.freeze({ status: 'accepted', changed: value.changed, partial: notes.some(n => n.code === 'SKIPPED'),
      detail: [value.done, ...notes.map(n => `${n.code}: ${n.detail}`)].join(' '), notes: Object.freeze(notes) });
  }

  function model({ call, execute = (_, work) => work(), changed = () => {} }) {
    let epoch = 0, disposed = false, reader = null;
    const state = { open: false, input: '', action: 'pause', amount: '5', suggestions: [], reading: false,
      writing: false, preview: null, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = v => !disposed && state.open && epoch === v;
    const invalidate = () => { ++epoch; state.preview = null; reader?.abort(); };
    const read = async task => {
      if (disposed || !state.open || state.reading || state.writing) return false;
      const version = ++epoch, abort = new AbortController(); reader = abort;
      state.reading = true; state.preview = null; state.message = ''; publish();
      try { return await task(version, abort.signal); }
      catch (error) { if (current(version)) state.message = errorText(error); return false; }
      finally { state.reading = false; reader = null; publish(); }
    };
    async function inspect(target, action, signal, valid) {
      const view = resolutionOf(await call('resolveRooms', { query: { target }, signal }), target);
      if (!valid()) return null;
      const sources = new Array(view.groups.length);
      if (action === 'resume') {
        let next = 0, failure = null;
        await Promise.all(Array.from({ length: Math.min(4, view.groups.length) }, async () => {
          while (valid() && !failure && next < view.groups.length) {
            const index = next++, group = view.groups[index];
            try {
              const room = queryOf(`${group.coordinator_room}@${group.household}`);
              sources[index] = resumeState(await call('state', { params: { room }, signal }), group);
            } catch (error) { failure = error; }
          }
        }));
        if (failure) throw failure;
      }
      if (!valid()) return null;
      const entries = action === 'resume' ? sources : [];
      return Object.freeze({ view, sources: Object.freeze(entries),
        signature: JSON.stringify([view.signature, entries.map(s => s.signature)]) });
    }
    return { state,
      open() { if (!disposed) { state.open = true; publish(); } },
      refresh() {
        if (!disposed) state.open = true;
        return read(async (v, signal) => {
          const suggestions = suggestionsOf(await call('setRooms', { signal }));
          if (!current(v)) return false;
          state.suggestions = suggestions; return true;
        });
      },
      edit(field, value) {
        if (disposed || state.writing || !['input', 'action', 'amount'].includes(field) || typeof value !== 'string') return false;
        if (field === 'action' && !Object.hasOwn(actions, value)) return false;
        invalidate(); state[field] = value; state.message = ''; publish(); return true;
      },
      review() {
        return read(async (v, signal) => {
          const target = queryOf(state.input), request = command(target, state.action, state.amount);
          const snapshot = await inspect(target, request.action, signal, () => current(v));
          if (!snapshot || !current(v)) return false;
          state.preview = Object.freeze({ ...snapshot, request }); return true;
        });
      },
      async confirm(preview) {
        if (disposed || !state.open || state.reading || state.writing || !preview || preview !== state.preview) return false;
        const version = ++epoch;
        state.preview = null; state.writing = true; state.message = 'Checking the complete room set…'; publish();
        let sent = false, accepted = false;
        try {
          await execute(preview.view.groups, async () => {
            if (sent || !current(version)) return;
            const fresh = await inspect(preview.view.target, preview.request.action, undefined, () => current(version));
            if (sent || !fresh || !current(version)) return;
            if (fresh.signature !== preview.signature) throw new Error('The resolved rooms, group identities, or resumed sources changed. Review the new target before acting.');
            sent = true;
            const result = outcomeOf(await call(preview.request.route, { body: preview.request.body }));
            state.result = result; accepted = true;
          });
          if (!sent) state.result = Object.freeze({ status: 'not_sent', detail: 'No command was sent. Review the target again.' });
        } catch (error) {
          if (!accepted) state.result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent', detail: `${errorText(error)} ${sent
            ? 'Some rooms may have changed, or the request may still finish. Check the house before another action; this request was not repeated.'
            : 'No command was sent.'}` });
        } finally { state.writing = false; state.message = ''; publish(); }
        return accepted;
      },
      cancel() { if (!disposed && !state.writing) { invalidate(); state.message = ''; publish(); } },
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; state.open = false; invalidate(); },
    };
  }

  // Acquire every affected group's existing lane before work, or none. A
  // collective command is one daemon request, never a loop of browser writes.
  function coordinated({ cards, refresh, begin, end, shape, levels }) {
    return async (groups, work) => {
      const find = () => groups.map(g => [...cards()].find(card => shape(card.zone) === g.shape));
      let targets = find();
      if (targets.some(p => !p)) { await refresh(); targets = find(); }
      if (targets.some(p => !p) || new Set(targets).size !== targets.length) throw new Error('An affected group changed. Refresh and review the target again.');
      const held = [];
      let releaseLevels = null;
      try {
        if (levels) {
          releaseLevels = levels.reserve(groups);
          if (!releaseLevels) throw new Error('A room has a pending volume or mute change. Wait for its result and review again.');
        }
        for (const target of targets) {
          if (!begin(target)) throw new Error('An affected group already has a playback action in progress. Nothing was sent.');
          held.push(target);
        }
        return await work();
      } finally {
        releaseLevels?.();
        for (const target of held.reverse()) { try { end(target); } catch (_) { /* Release the remaining lanes too. */ } }
      }
    };
  }
  // Use the native slider map for exclusion, including cards replaced by a
  // regroup. Register observe() as a card hook so newly created mute controls
  // inherit the same pending operation; no queued slider draft is consumed.
  function levelLanes({ cards, flights }) {
    const owners = new Set();
    function protect(entry, owner) {
      if (!owners.has(owner) || owner.entries.has(entry)) return;
      owner.entries.set(entry, { mutedPending: entry.mutedPending, muteDisabled: entry.mute.disabled });
      ++entry.version;
      entry.input.disabled = entry.mute.disabled = true;
      entry.mutedPending = true;
    }
    return {
      observe(card) {
        for (const entry of card.sliders.values()) protect(entry, flights.get(entry.room));
      },
      reserve(groups) {
        const addresses = new Set(groups.flatMap(g => g.members.map(roomName)));
        const entries = [...cards()].flatMap(card => [...card.sliders.values()])
          .filter(entry => addresses.has(entry.room));
        if ([...addresses].some(room => flights.has(room) || !entries.some(entry => entry.room === room))
            || entries.some(entry => entry.timer !== null || entry.pendingVolume !== null
              || entry.volumeWriting || entry.mutedPending)) return null;
        const owner = { entries: new Map() }; owners.add(owner);
        for (const room of addresses) flights.set(room, owner);
        for (const entry of entries) protect(entry, owner);
        let released = false;
        return () => {
          if (released) return; released = true; owners.delete(owner);
          for (const room of addresses) if (flights.get(room) === owner) flights.delete(room);
          for (const [entry, before] of owner.entries) {
            ++entry.version;
            entry.mutedPending = before.mutedPending;
            entry.mute.disabled = before.muteDisabled;
            // endAction requests fresh levels for every surviving/split card.
            // Do not restore an old displayed volume as a current observation.
            entry.input.disabled = true;
          }
        };
      },
    };
  }
  return { routes, actions, resolutionOf, suggestionsOf, command, resumeState, outcomeOf, model, coordinated, levelLanes, transport, roomName, MAX_ROOMS };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = RoomSetsRemote;
