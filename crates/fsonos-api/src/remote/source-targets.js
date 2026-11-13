// One reviewed source request for independent, authoritatively resolved groups.
'use strict';
const SourceTargetsRemote = (() => {
  const R = typeof RoomSetsRemote !== 'undefined' ? RoomSetsRemote : require('./room-sets.js');
  const routes = { sourceTargetRooms: ['GET', '/rooms'], sourceTargetResolve: ['GET', '/rooms/resolve'],
    sourceFavorites: ['GET', '/favorites'], sourceFavoritePlay: ['POST', '/play/favorite'] };
  const text = (value, max = 65536) => typeof value === 'string' && value.trim() !== ''
    && value.length <= max && !/[\p{Cc}]/u.test(value);
  const key = (household, coordinator) => JSON.stringify([household, coordinator]);
  const fail = message => { throw new Error(message); };
  function query(value) {
    if (!text(value, 1024) || [...value.trim()].length > 128) fail('Enter a room, alias, here, all@household, or everywhere (at most 128 characters).');
    return value.trim();
  }

  // The daemon's exact-title stage uses fsonos_types::text::normalize. Keep its
  // Latin folds and accidental names here; do not guess its fuzzy/prefix stage.
  function nameKey(value) {
    const folds = [['àáâãäåāăą', 'a'], ['æ', 'ae'], ['çćč', 'c'], ['ďđð', 'd'], ['èéêëēėęě', 'e'],
      ['ìíîïīįı', 'i'], ['łľĺ', 'l'], ['ñńň', 'n'], ['òóôõöøōő', 'o'], ['œ', 'oe'], ['řŕ', 'r'],
      ['śšşș', 's'], ['ß', 'ss'], ['ťţț', 't'], ['ùúûüūůűų', 'u'], ['ýÿ', 'y'], ['źżž', 'z'], ['þ', 'th']];
    return [...value.toLowerCase()].map(c => folds.find(([letters]) => letters.includes(c))?.[1]
      ?? (c === '♭' ? ' flat ' : c === '♯' ? ' sharp ' : /[\p{L}\p{N}]/u.test(c) ? c : ' ')).join('').trim().replace(/ +/g, ' ');
  }
  function prepareNative(kind, raw, mode) {
    if (!['playlist', 'favorite'].includes(kind) || !['play', 'next', 'end'].includes(mode)) fail('Choose a supported source and action.');
    if (!text(raw, 1024) || [...raw.trim()].length > 256 || !nameKey(raw)) fail('Enter the complete saved playlist or favorite title (at most 256 characters).');
    const title = raw.trim();
    if (/^\+?\d+$/.test(title) || /^(?:SQ:|FV:)/i.test(title)) fail('Use the full title for a room set. Native IDs and numbered positions belong to one household.');
    return Object.freeze({ item: Object.freeze({ kind: kind + '_query', query: title, container: true }), title, mode });
  }
  const native = item => ['playlist_query', 'favorite_query'].includes(item.kind);
  function nativeItem(value, item, mode, player) {
    const favorite = item.kind === 'favorite_query';
    const list = favorite ? value : player.playlists(value);
    if (!Array.isArray(list) || list.length > 10000 || list.some(v => !v || !text(v.id, 256)
        || !text(v.title, 4096) || (favorite && !['track', 'stream', 'container', 'unplayable'].includes(v.kind)))
        || new Set(list.map(v => v.id)).size !== list.length) fail('A household source catalog is incomplete. Read the sources again.');
    const matches = list.filter(v => nameKey(v.title) === nameKey(item.query));
    if (matches.length !== 1) fail(`The title “${item.query}” must identify exactly one ${favorite ? 'favorite' : 'saved Sonos playlist'} in every selected household.`);
    const selected = matches[0];
    if (selected.kind === 'unplayable') fail(`“${selected.title}” is a browse shortcut and cannot play.`);
    if (mode !== 'play' && selected.kind === 'stream') fail(`“${selected.title}” is a live stream. Choose Play now; streams cannot join a queue.`);
    return Object.freeze({ kind: favorite ? 'favorite' : 'sonos_playlist', id: selected.id, title: selected.title,
      sourceKind: favorite ? selected.kind : 'container', container: !favorite || selected.kind === 'container' });
  }

  async function bounded(items, read, valid) {
    const result = new Array(items.length); let next = 0, failure = null;
    await Promise.all(Array.from({ length: Math.min(4, items.length) }, async () => {
      try {
        while (!failure && next < items.length) {
          valid(); const index = next++; result[index] = await read(items[index]); valid();
        }
      } catch (error) { failure ||= error; }
    }));
    if (failure) throw failure;
    return result;
  }
  async function inspect({ call, player, target, item, mode, signal, valid = () => {} }) {
    target = query(target);
    if (!['play', 'next', 'end'].includes(mode) || !item || ['line_in', 'sonos_playlist_track', 'sonos_playlist'].includes(item.kind)) {
      fail('Choose a source link or a complete household source title for this room set.');
    }
    if (mode !== 'play' && item.kind === 'stream') fail('Live streams can play now but cannot join a queue.');
    const read = async (route, options = {}) => {
      valid(); const value = await call(route, { ...options, ...(signal ? { signal } : {}) }); valid(); return value;
    };
    const resolve = async () => R.resolutionOf(await read('sourceTargetResolve', { query: { target } }), target);
    const resolution = await resolve();
    const groups = Object.freeze(resolution.groups.map(g => Object.freeze({
      target: `${g.coordinator_room}@${g.household}`, household: g.household, coordinator: g.coordinator_room,
      coordinator_id: g.coordinator, label: `${g.members.map(r => r.room).join(' + ')} (${g.household})`,
      signature: g.shape, shape: g.shape, members: g.members,
    })));
    const sources = new Map();
    if (native(item)) {
      const homes = [...new Map(groups.map(g => [g.household, g])).values()];
      await bounded(homes, async group => {
        const value = await read(item.kind === 'favorite_query' ? 'sourceFavorites' : 'sourcePlaylists', { query: { zone: group.target } });
        try { sources.set(group.household, nativeItem(value, item, mode, player)); }
        catch (error) { fail(`${group.household}: ${error.message}`); }
      }, valid);
    }
    const entries = Object.freeze(await bounded(groups, async group => {
      const selected = sources.get(group.household) || item;
      const observed = player.playback(await read('state', { params: { room: group.target } }), group);
      let before = observed;
      if (mode !== 'play' || selected.container) {
        const queue = player.queue(await read('sourceQueueState', {
          params: { room: group.target }, query: { offset: 0, limit: 1 },
        }), group);
        before = Object.freeze({ ...observed, queue, signature: JSON.stringify([
          queue.revision, queue.total, ...(mode !== 'end' ? [queue.playing, observed.signature] : []),
        ]) });
      }
      return Object.freeze({ group, item: selected, before,
        sourceLabel: selected.id ? `${selected.title} (${selected.id})` : selected.uri });
    }, valid));
    if ((await resolve()).signature !== resolution.signature) fail('The resolved room set changed during review. Review its current groups again.');
    return Object.freeze({ target, item, mode, resolution, groups, entries, signature: JSON.stringify([
      resolution.signature, entries.map(e => [e.group.household, e.group.coordinator_id, e.item, e.before.signature]),
    ]) });
  }

  function request(choice, target, player) {
    target = query(target);
    if (!native(choice.item)) return Object.freeze({ route: choice.mode === 'play' ? 'sourcePlay' : 'sourceQueue',
      body: Object.freeze(player.commandBody(choice, target)) });
    const favorite = choice.item.kind === 'favorite_query';
    return Object.freeze({ route: choice.mode === 'play' ? favorite ? 'sourceFavoritePlay' : 'sourcePlaylistPlay' : 'sourceQueue',
      body: Object.freeze({ zone: target, [favorite ? 'favorite' : 'playlist']: choice.item.query,
        ...(choice.mode === 'play' ? {} : { next: choice.mode === 'next' }) }) });
  }

  function result(value, reviewed, player) {
    if (reviewed.mode === 'play') {
      const detail = player.outcome(value);
      const partial = (value.notes || []).some(note => note.code === 'SKIPPED');
      return Object.freeze({ status: 'accepted', detail, partial, complete: !partial });
    }
    const groups = reviewed.groups;
    if (groups.length === 1 && value && !Object.hasOwn(value, 'added') && !Object.hasOwn(value, 'failed')) {
      return Object.freeze({ status: 'accepted', detail: player.queued(value, groups[0]), partial: false, complete: true });
    }
    const bad = () => fail('The daemon did not return one complete queue receipt for every reviewed group. Read their queues before another add; this request was not repeated.');
    if (!value || !text(value.done) || typeof value.complete !== 'boolean' || !Array.isArray(value.added)
        || !Array.isArray(value.failed) || typeof value.logged !== 'boolean' || typeof value.undoable !== 'boolean'
        || value.complete !== (value.failed.length === 0) || (value.undoable && !value.logged)
        || value.added.length + value.failed.length !== groups.length) bad();
    const expected = new Map(groups.map(g => [key(g.household, g.coordinator_id), g])), seen = new Set();
    const claim = row => {
      if (!row || !text(row.household, 4096) || !text(row.coordinator, 256)) bad();
      const id = key(row.household, row.coordinator), group = expected.get(id);
      if (!group || seen.has(id)) bad(); seen.add(id); return group;
    };
    const details = [value.done];
    for (const added of value.added) {
      const group = claim(added);
      details.push(`${group.coordinator}@${group.household}: ${player.queued(added.queue, group)}`);
    }
    for (const failed of value.failed) {
      const group = claim(failed), error = failed.error;
      if (failed.zone !== group.coordinator || !error || !text(error.code, 256) || !text(error.detail)
          || typeof error.retryable !== 'boolean' || error.retryable || (error.hint != null && !text(error.hint))) bad();
      details.push(`${group.coordinator}@${group.household}: ${error.code}: ${error.detail}${error.hint ? ' ' + error.hint : ''}`);
    }
    if (!value.logged) details.push('The action was not logged; no batch undo is available.');
    else if (!value.undoable) details.push('No safe batch undo is available. Inspect the queues before another add.');
    if (value.failed.length) details.push('Check each failed group before another action. Do not repeat the whole batch.');
    return Object.freeze({ status: 'accepted', detail: details.join('\n'), partial: !value.complete, complete: value.complete,
      logged: value.logged, undoable: value.undoable });
  }
  return { routes, query, nameKey, prepareNative, inspect, request, result, suggestions: R.suggestionsOf };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = SourceTargetsRemote;
